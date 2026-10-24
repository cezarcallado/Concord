//! The right-click menu of a person (v0.6): one builder used everywhere a person or a stream
//! appears, so the same person gets the same menu wherever they are clicked.
//!
//! Two halves:
//!
//! - the **model** ([`Facts`] -> [`rows`]): a pure function from "what is true right now" (me or
//!   somebody else, same voice room, live, camera on, roles, blocked...) to the list of rows
//!   (separators, section labels and items, each possibly disabled with a reason). No egui, fully
//!   unit tested: it returns item ids.
//! - the **app side** (`impl App`): collects the facts from the app's state, draws the rows with
//!   egui and runs the chosen action. The menu itself is one floating popup owned by the app
//!   ([`CtxMenuUi::open`], drawn once per frame after the panels), opened by a secondary click on
//!   any person/stream widget ([`App::menu_hit`]); Esc or a click outside closes it. Moderation
//!   actions that cannot be taken back (disconnect, role change, clearing a profile) ask first.
//!
//! Left-click behaviours are untouched (profile card on names, pin on stage cards, double-click
//! fullscreen). A menu opened from a stage card starts with the card's own items ("Fixar",
//! "Ocultar este card", `call::card_menu_section`); that "Fixar" / "Desafixar" is then its only pin
//! item ([`card_rows`]). Native video child windows hide while the menu overlaps them like for any
//! other popup (see `call::sync_video`, "airspace").
//!
//! v1.0: the kit's popup and rows (`ui_kit::menu_*`): Phosphor icons, hairline separators between
//! the groups, section headings in sentence case, what cannot be undone in "ao vivo" red, and the
//! confirmations in the kit's dialog (the action on the right).

use std::collections::HashSet;

use eframe::egui::{self, Align, Layout, Pos2, RichText};
use vox_client_core::VoiceEffect;
use vox_proto::{ChannelId, ChannelKind, ModClearProfileRequest, Presence, Role, User, UserId};

use crate::app::{App, Center};
use crate::backend::Cmd;
use crate::theme::{self, ph, space};
use crate::ui_kit::{self, Kind, Size};
use crate::profile::{presence_color, presence_hint, presence_label, ProfileCmd};
use crate::roles::{role_label, AdminCmd};

// ---------------------------------------------------------------------------------------------
// The model (pure)
// ---------------------------------------------------------------------------------------------

/// Where the menu was opened. Only a few things depend on it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Origin {
    #[default]
    Sidebar,
    Online,
    Tile,
    Dm,
    Chat,
    /// The "…" of the profile card: "Ver perfil" is not offered (it is open).
    Popout,
}

/// Which section goes first: a stream or camera tile leads with its own section.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lead {
    #[default]
    None,
    Stream,
    Camera,
}

/// An item of the menu. The same id means the same action wherever the menu is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Id {
    // Person
    ViewProfile,
    EditProfile,
    SendMessage,
    Call,
    CopyUsername,
    // Voice (same room)
    Volume,
    MuteLocal,
    // Their stream
    StreamWatch,
    StreamStop,
    StreamVolume,
    StreamMute,
    StreamFocus,
    StreamFullscreen,
    StreamPopOut,
    // Their camera
    CamFocus,
    CamHide,
    CamShow,
    // Moderation
    ModServerMute,
    ModServerUnmute,
    ModDisconnect,
    ModMove,
    ModStopStream,
    ModSetRole,
    ModRemovePhoto,
    ModClearProfile,
    // DMs
    Block,
    Unblock,
    // My own entry
    Presence,
    MuteMic,
    Deafen,
    CameraToggle,
    CameraSettings,
    VoiceFx,
    SwapScreen,
    StopShare,
    SelfPreviewFocus,
    SelfPreviewToggle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Sep,
    Label(&'static str),
    /// An item; `Some(reason)` = shown disabled with that tooltip.
    Item(Id, Option<&'static str>),
}

/// Everything the menu depends on. The app fills it in ([`App::menu_facts`]); tests build it by hand.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub origin: Origin,
    pub lead: Lead,
    /// The menu is about myself.
    pub me: bool,
    pub hub_up: bool,
    /// This hub has profile cards (presence, status, "Editar perfil").
    pub profile_ok: bool,
    pub dms_ok: bool,
    /// I am in a private call with this person right now.
    pub in_call_with: bool,
    pub blocked: bool,
    /// They are in the voice room I am in.
    pub same_room: bool,
    /// They are in some voice channel (the hub's voice state).
    pub in_voice: bool,
    pub server_muted: bool,
    /// There is another voice channel to move them to.
    pub other_channels: bool,
    pub live: bool,
    pub audio_only: bool,
    pub watching: bool,
    pub popped: bool,
    pub focused: bool,
    pub camera_on: bool,
    pub camera_hidden: bool,
    pub cam_focused: bool,
    pub roles_ok: bool,
    pub mine: Role,
    pub theirs: Role,
    pub has_avatar: bool,
    /// There is something on their profile a moderator could clear (or we do not know yet).
    pub clearable: bool,
    // --- my own entry
    pub presence_ok: bool,
    pub my_in_voice: bool,
    pub my_no_mic: bool,
    pub my_muted: bool,
    pub my_deafened: bool,
    pub my_server_muted: bool,
    pub my_ptt: bool,
    pub my_cam_on: bool,
    pub my_cam_starting: bool,
    pub my_in_dm_call: bool,
    pub switching: bool,
    pub preview_on: bool,
}

const OUTRANKED: &str = "Esse cargo é igual ou acima do seu";
const NEED_ADMIN: &str = "Precisa ser Admin";
const NEED_MOD: &str = "Precisa ser Moderador";
const OFFLINE: &str = "Sem conexão com o servidor";

enum Sec {
    Profile,
    Voice,
    Stream,
    Camera,
    Moderation,
    Block,
}

fn item(id: Id) -> Row {
    Row::Item(id, None)
}

fn off(id: Id, reason: &'static str) -> Row {
    Row::Item(id, Some(reason))
}

/// The rows of the menu for `f`: sections separated by [`Row::Sep`], only what applies.
pub fn rows(f: &Facts) -> Vec<Row> {
    let mut secs = if f.me { my_sections(f) } else { person_sections(f) };
    secs.retain(|(_, v)| v.iter().any(|r| matches!(r, Row::Item(..))));
    let lead = match f.lead {
        Lead::Stream => Some(Sec::Stream),
        Lead::Camera => Some(Sec::Camera),
        Lead::None => None,
    };
    if let Some(l) = lead {
        if let Some(i) = secs.iter().position(|(s, _)| std::mem::discriminant(s) == std::mem::discriminant(&l)) {
            let s = secs.remove(i);
            secs.insert(0, s);
        }
    }
    let mut out = Vec::new();
    for (n, (_, v)) in secs.into_iter().enumerate() {
        if n > 0 {
            out.push(Row::Sep);
        }
        out.extend(v);
    }
    out
}

/// Just the ids, in order (what the tests compare).
pub fn ids(rows: &[Row]) -> Vec<Id> {
    rows.iter().filter_map(|r| if let Row::Item(id, _) = r { Some(*id) } else { None }).collect()
}

/// The ids that can be clicked.
pub fn enabled_ids(rows: &[Row]) -> Vec<Id> {
    rows.iter().filter_map(|r| if let Row::Item(id, None) = r { Some(*id) } else { None }).collect()
}

/// The reason an item is disabled, if it is.
pub fn disabled_reason(rows: &[Row], id: Id) -> Option<&'static str> {
    rows.iter().find_map(|r| if let Row::Item(i, Some(why)) = r { (*i == id).then_some(*why) } else { None })
}

fn person_sections(f: &Facts) -> Vec<(Sec, Vec<Row>)> {
    let mut secs = Vec::new();

    // 1. Profile / message / call / copy.
    let mut p = Vec::new();
    if f.origin != Origin::Popout {
        p.push(item(Id::ViewProfile));
    }
    if f.dms_ok {
        p.push(item(Id::SendMessage));
        if !f.in_call_with {
            p.push(if !f.hub_up {
                off(Id::Call, OFFLINE)
            } else if f.blocked {
                off(Id::Call, "Você bloqueou esta pessoa: desbloqueie para ligar")
            } else {
                item(Id::Call)
            });
        }
    }
    p.push(item(Id::CopyUsername));
    secs.push((Sec::Profile, p));

    // 2. Voice: only for somebody in my room.
    if f.same_room {
        secs.push((Sec::Voice, vec![item(Id::Volume), item(Id::MuteLocal)]));
    }

    // 3. Their stream.
    if f.live {
        let mut s = vec![Row::Label("Transmissão")];
        if f.watching {
            s.push(item(Id::StreamStop));
            s.push(item(Id::StreamVolume));
            s.push(item(Id::StreamMute));
            if !f.audio_only {
                s.push(item(Id::StreamFocus));
                s.push(if f.popped { off(Id::StreamFullscreen, "Está em outra janela: traga de volta para usar a tela cheia") } else { item(Id::StreamFullscreen) });
                s.push(item(Id::StreamPopOut));
            }
        } else {
            s.push(if f.same_room { item(Id::StreamWatch) } else { off(Id::StreamWatch, "Entre no canal de voz desta pessoa para assistir") });
        }
        secs.push((Sec::Stream, s));
    }

    // 4. Their camera (the video exists only in the room I am in).
    if f.camera_on && f.same_room {
        let mut c = vec![Row::Label("Câmera")];
        if f.camera_hidden {
            c.push(item(Id::CamShow));
        } else {
            c.push(item(Id::CamFocus));
            c.push(item(Id::CamHide));
        }
        secs.push((Sec::Camera, c));
    }

    // 5. Moderation, by role.
    if f.roles_ok && f.mine.can_moderate() {
        let outranks = f.mine.outranks(f.theirs);
        let why = (!outranks).then_some(OUTRANKED);
        let mut m = Vec::new();
        if f.in_voice {
            m.push(Row::Item(if f.server_muted { Id::ModServerUnmute } else { Id::ModServerMute }, why));
            m.push(Row::Item(Id::ModDisconnect, why));
            if f.other_channels {
                m.push(Row::Item(Id::ModMove, why));
            }
            if f.live {
                m.push(Row::Item(Id::ModStopStream, why));
            }
        }
        let role_ok = f.mine.can_admin() && f.theirs != Role::Owner && (f.mine == Role::Owner || f.theirs < f.mine);
        if role_ok {
            m.push(item(Id::ModSetRole));
        } else if !m.is_empty() || f.mine.can_admin() {
            m.push(off(Id::ModSetRole, if f.mine.can_admin() { OUTRANKED } else { NEED_ADMIN }));
        }
        if f.has_avatar && outranks {
            m.push(item(Id::ModRemovePhoto));
        }
        if f.profile_ok && outranks {
            m.push(if f.clearable { item(Id::ModClearProfile) } else { off(Id::ModClearProfile, "Não há nada para limpar neste perfil") });
        }
        if !m.is_empty() {
            m.insert(0, Row::Label("Moderação"));
        }
        secs.push((Sec::Moderation, m));
    } else if f.roles_ok && f.in_voice && f.live {
        // A member sees that stopping somebody's stream exists, and who may do it.
        secs.push((Sec::Moderation, vec![Row::Label("Moderação"), off(Id::ModStopStream, NEED_MOD)]));
    }

    // 6. Block (DMs).
    if f.dms_ok {
        secs.push((Sec::Block, vec![item(if f.blocked { Id::Unblock } else { Id::Block })]));
    }
    secs
}

fn my_sections(f: &Facts) -> Vec<(Sec, Vec<Row>)> {
    let mut secs = Vec::new();
    let mut p = Vec::new();
    if f.origin != Origin::Popout {
        p.push(item(Id::ViewProfile));
    }
    if f.profile_ok {
        p.push(item(Id::EditProfile));
    }
    p.push(item(Id::CopyUsername));
    if f.presence_ok {
        p.push(item(Id::Presence));
    }
    secs.push((Sec::Profile, p));

    if f.my_in_voice {
        let mut v = Vec::new();
        v.push(if f.my_server_muted {
            off(Id::MuteMic, "Um moderador silenciou você no servidor: só um moderador pode liberar")
        } else if f.my_no_mic {
            off(Id::MuteMic, "Sem microfone")
        } else if f.my_ptt {
            off(Id::MuteMic, "Push-to-talk ativo: a tecla decide")
        } else if f.my_deafened {
            off(Id::MuteMic, "Você está ensurdecido")
        } else {
            item(Id::MuteMic)
        });
        v.push(item(Id::Deafen));
        v.push(if f.my_cam_starting {
            off(Id::CameraToggle, "Ligando a câmera...")
        } else if !f.my_cam_on && crate::platform::camera_soon() {
            off(Id::CameraToggle, "Câmera: em breve no Mac")
        } else {
            item(Id::CameraToggle)
        });
        v.push(item(Id::CameraSettings));
        if !f.my_no_mic && !f.my_in_dm_call {
            v.push(item(Id::VoiceFx));
        }
        secs.push((Sec::Voice, v));
    }

    if f.live {
        let mut s = vec![Row::Label("Minha transmissão")];
        s.push(if f.switching { off(Id::SwapScreen, "Trocando...") } else { item(Id::SwapScreen) });
        s.push(item(Id::StopShare));
        if !f.audio_only {
            if f.preview_on {
                s.push(item(Id::SelfPreviewFocus));
            }
            s.push(item(Id::SelfPreviewToggle));
        }
        secs.push((Sec::Stream, s));
    }
    secs
}

// ---------------------------------------------------------------------------------------------
// The app side
// ---------------------------------------------------------------------------------------------

/// A confirmation the menu asks before something that cannot be taken back.
#[derive(Clone, Debug, PartialEq)]
pub enum Confirm {
    Disconnect(User),
    SetRole { user: User, role: Role },
    Clear { user: User, what: ModClearProfileRequest, has_bio: bool, has_status: bool, has_name: bool },
}

/// What a click on the menu asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    Item(Id),
    MoveTo(ChannelId),
    SetRole(Role),
    Presence(Presence),
    Effect(VoiceEffect),
}

/// The menu that is open.
#[derive(Clone, Debug)]
pub struct OpenMenu {
    pub user: User,
    pub origin: Origin,
    pub lead: Lead,
    pub pos: Pos2,
    seq: u64,
}

#[derive(Default)]
pub struct CtxMenuUi {
    pub open: Option<OpenMenu>,
    seq: u64,
    pub confirm: Option<Confirm>,
    /// Cameras I chose to hide for myself this session (voice identity = user id).
    pub hidden_cams: HashSet<UserId>,
    /// The rows of the menu as drawn last (autotest).
    pub last_rows: Vec<Row>,
    /// Menus opened and actions run so far (autotest).
    pub opened: u32,
    pub actions: u32,
}

/// The pin items of the person's sections. A menu opened from a stage card has one pin item, the
/// card's own (`call::card_menu_section`), so these are left out there.
const PIN_ITEMS: [Id; 3] = [Id::StreamFocus, Id::CamFocus, Id::SelfPreviewFocus];

/// The rows drawn when the menu was opened from a stage card: the person's rows without their pin
/// items (the card's own "Fixar" / "Desafixar" is the only one), and no separator left dangling.
pub fn card_rows(rows: &[Row]) -> Vec<Row> {
    let kept: Vec<Row> = rows.iter().copied().filter(|r| !matches!(r, Row::Item(id, _) if PIN_ITEMS.contains(id))).collect();
    let mut out: Vec<Row> = Vec::with_capacity(kept.len());
    for r in kept {
        if r == Row::Sep && matches!(out.last(), None | Some(Row::Sep)) {
            continue;
        }
        out.push(r);
    }
    while out.last() == Some(&Row::Sep) {
        out.pop();
    }
    out
}

fn close_menu(ui: &egui::Ui) {
    egui::Popup::close_all(ui.ctx());
}

impl App {
    /// A secondary click on a person's (or stream's) widget opens their menu at the pointer.
    pub(crate) fn menu_hit(&mut self, resp: &egui::Response, user: &User, origin: Origin, lead: Lead) {
        if resp.secondary_clicked() {
            let pos = resp.ctx.pointer_interact_pos().or_else(|| resp.ctx.pointer_latest_pos()).unwrap_or(resp.rect.center());
            self.open_person_menu(&resp.ctx, user, origin, lead, pos);
        }
    }

    pub(crate) fn open_person_menu(&mut self, ctx: &egui::Context, user: &User, origin: Origin, lead: Lead, pos: Pos2) {
        if self.screen != crate::app::Screen::Main {
            return;
        }
        // (A card of the stage sets its own right after: `call::card_menu_hit`.)
        self.call.menu_card = None;
        let m = &mut self.cmenu;
        m.seq += 1;
        m.opened += 1;
        let seq = m.seq;
        m.open = Some(OpenMenu { user: user.clone(), origin, lead, pos, seq });
        egui::Popup::open_id(ctx, egui::Id::new(("person_menu", seq)));
        ctx.request_repaint();
    }

    /// Draw the open menu (once per frame, after the panels so it is on top).
    pub fn person_menu_layer(&mut self, ctx: &egui::Context) {
        let Some(open) = self.cmenu.open.clone() else { return };
        let id = egui::Id::new(("person_menu", open.seq));
        // The person may have changed (a new role, a new picture): use the freshest copy.
        let user = self.known_user(open.user.id).unwrap_or_else(|| open.user.clone());
        let shown = egui::Popup::new(id, ctx.clone(), egui::PopupAnchor::Position(open.pos), egui::LayerId::background())
            .kind(egui::PopupKind::Menu)
            .layout(Layout::top_down_justified(Align::Min))
            .frame(ui_kit::popup_frame(&ctx.global_style()))
            .style(ui_kit::menu_style)
            .gap(0.0)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .open_memory(None)
            .show(|ui| self.person_menu_ui(ui, &user, open.origin, open.lead, open.pos));
        if shown.is_none() || !egui::Popup::is_id_open(ctx, id) {
            self.cmenu.open = None;
        }
    }

    // ------------------------------------------------------------ facts

    /// Everything the menu for `user` depends on, from the app's state.
    pub(crate) fn menu_facts(&self, user: &User, origin: Origin, lead: Lead) -> Facts {
        let me = self.my_id() == Some(user.id);
        let key = user.id.to_string();
        let hub_vm = self.voice_members.get(&user.id).filter(|m| m.channel_id.is_some());
        let here = self.members_here().into_iter().find(|m| m.user.id == user.id).cloned();
        let vm = here.clone().or_else(|| hub_vm.cloned());
        let same_room = !me && here.is_some();
        let mine = self.my_role();
        let theirs = self.role_of(user.id);
        let live = if me { self.my_share_live() } else { vm.as_ref().is_some_and(|m| m.streaming) };
        let audio_only = vm.as_ref().is_some_and(|m| self.stream_is_audio_only(m));
        let camera_on = if me { self.camera_on() } else { vm.as_ref().is_some_and(|m| self.camera_flag_of(m)) };
        let other_channels = self.channels.iter().any(|c| c.kind == ChannelKind::Voice && Some(c.id) != hub_vm.and_then(|m| m.channel_id));
        let clearable = self.profile_clear_parts(user).is_none_or(|(bio, status)| bio || status || user.display_name.is_some());
        let cam_starting = self.voice.camera == crate::app::CameraUi::Starting;
        Facts {
            origin,
            lead,
            me,
            hub_up: self.hub_up,
            profile_ok: self.profile.supported != Some(false),
            dms_ok: !me && !(self.dms.listed && !self.dms.supported),
            in_call_with: self.in_dm_call() && self.dms.voice_dm.and_then(|d| self.dms.conv(d)).is_some_and(|c| c.peer.id == user.id),
            blocked: self.dms.is_blocked(user.id),
            same_room,
            in_voice: hub_vm.is_some(),
            server_muted: hub_vm.is_some_and(|m| m.server_muted),
            other_channels,
            live,
            audio_only: if me { self.voice.share_audio_only } else { audio_only },
            watching: self.is_watching(&key),
            popped: self.stream_popped(&key),
            focused: self.is_pinned(crate::call::TileKey::Stream(user.id)),
            camera_on,
            camera_hidden: self.cmenu.hidden_cams.contains(&user.id),
            cam_focused: self.is_pinned(crate::call::TileKey::Member(user.id)),
            roles_ok: self.roles.supported,
            mine,
            theirs,
            has_avatar: user.avatar.is_some(),
            clearable,
            presence_ok: self.profile.supported == Some(true),
            my_in_voice: self.voice.channel.is_some(),
            my_no_mic: self.voice.no_mic,
            my_muted: self.voice.muted,
            my_deafened: self.voice.deafened,
            my_server_muted: self.i_am_server_muted(),
            my_ptt: self.ptt_active(),
            my_cam_on: self.camera_on(),
            my_cam_starting: cam_starting,
            my_in_dm_call: self.in_dm_call(),
            switching: self.voice.switching_to.is_some(),
            preview_on: self.settings.show_self_preview,
        }
    }

    /// The rows for `user` (what [`App::person_menu_ui`] draws).
    pub fn menu_rows(&self, user: &User, origin: Origin, lead: Lead) -> Vec<Row> {
        rows(&self.menu_facts(user, origin, lead))
    }

    // ------------------------------------------------------------ drawing

    /// The menu's content. Also usable inside any menu-like `Ui`. I6: it scrolls when the app's
    /// window is shorter than the menu (a stage card's menu is long), so no item is out of reach.
    pub(crate) fn person_menu_ui(&mut self, ui: &mut egui::Ui, user: &User, origin: Origin, lead: Lead, pos: Pos2) {
        let max_h = (ui.ctx().content_rect().height() - 24.0).max(120.0);
        egui::ScrollArea::vertical().id_salt("person_menu_scroll").max_height(max_h).auto_shrink([true, true]).show(ui, |ui| {
            self.person_menu_body(ui, user, origin, lead, pos);
        });
    }

    fn person_menu_body(&mut self, ui: &mut egui::Ui, user: &User, origin: Origin, lead: Lead, pos: Pos2) {
        let f = self.menu_facts(user, origin, lead);
        let mut list = rows(&f);
        ui.set_min_width(250.0);
        // (And at most this wide: the rows take the width they are given, and a right-aligned part
        // of the header stretched the menu to 600 points; v1.0 phase 4.)
        ui.set_max_width(280.0);
        ui.spacing_mut().item_spacing.y = 1.0;
        self.menu_header(ui, user, &f);
        ui_kit::menu_separator(ui);
        // A stage card's own items first ("Fixar", "Ocultar este card"...: `call.rs`); its pin is
        // then the menu's only one.
        if origin == Origin::Tile {
            match self.card_menu_section(ui, user, lead) {
                Some(true) => {
                    close_menu(ui);
                    self.cmenu.open = None;
                    return;
                }
                Some(false) => list = card_rows(&list),
                None => {}
            }
        }
        self.cmenu.last_rows = list.clone();
        let mut act: Option<Act> = None;
        for row in list {
            match row {
                Row::Sep => ui_kit::menu_separator(ui),
                Row::Label(t) => ui_kit::menu_heading(ui, t),
                Row::Item(id, why) => self.menu_item(ui, user, &f, id, why, &mut act),
            }
        }
        if let Some(a) = act {
            let ctx = ui.ctx().clone();
            close_menu(ui);
            self.cmenu.open = None;
            self.run_menu_act(&ctx, user, a, pos);
        }
    }

    /// The menu's head: the person's picture, name (in the role's colour) and badge, and the
    /// username under it when they have a display name.
    fn menu_header(&mut self, ui: &mut egui::Ui, user: &User, f: &Facts) {
        let role = if f.roles_ok { user.role } else { Role::Member };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = space::M;
            ui.add_space(space::S);
            crate::avatars::avatar_widget(&mut self.avatars.store, ui, user, 28.0, crate::avatars::Surface::Chat);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.add(egui::Label::new(crate::roles::role_name(user.shown_name(), role, Some(theme::GIZ)).font(theme::semibold(14.0))).truncate().selectable(false));
                    crate::roles::role_badge(ui, role);
                    if f.me {
                        ui.label(RichText::new("(você)").font(theme::body(12.0)).color(theme::NEVOA));
                    }
                });
                if user.has_display_name() {
                    ui.label(RichText::new(format!("@{}", user.username)).font(theme::body(12.0)).color(theme::NEVOA));
                }
            });
        });
    }

    fn menu_item(&mut self, ui: &mut egui::Ui, user: &User, f: &Facts, id: Id, why: Option<&'static str>, act: &mut Option<Act>) {
        let mut plain = |ui: &mut egui::Ui, icon: &str, text: &str, danger: bool| {
            if ui_kit::menu_item(ui, icon, text, danger, why) {
                *act = Some(Act::Item(id));
            }
        };
        match id {
            Id::ViewProfile => plain(ui, ph::USER, "Ver perfil", false),
            Id::EditProfile => plain(ui, ph::PENCIL_SIMPLE, "Editar perfil", false),
            Id::SendMessage => plain(ui, ph::CHAT_CIRCLE, "Enviar mensagem", false),
            Id::Call => plain(ui, ph::PHONE, "Ligar", false),
            Id::CopyUsername => plain(ui, ph::COPY, "Copiar nome de usuário", false),
            Id::Volume | Id::MuteLocal => self.menu_voice_controls(ui, user, id),
            Id::StreamVolume | Id::StreamMute => self.menu_stream_controls(ui, user, id),
            Id::StreamWatch => plain(ui, ph::MONITOR, if f.audio_only { "Ouvir" } else { "Assistir" }, false),
            Id::StreamStop => plain(ui, ph::STOP, if f.audio_only { "Parar de ouvir" } else { "Parar de assistir" }, false),
            Id::StreamFocus => plain(ui, ph::PUSH_PIN, if f.focused { "Desafixar transmissão" } else { "Fixar transmissão" }, false),
            Id::StreamFullscreen => plain(ui, ph::CORNERS_OUT, "Tela cheia", false),
            Id::StreamPopOut => plain(ui, ph::ARROW_SQUARE_OUT, if f.popped { "Trazer de volta" } else { "Abrir em janela" }, false),
            Id::CamFocus => plain(ui, ph::PUSH_PIN, if f.cam_focused { "Desafixar câmera" } else { "Fixar câmera" }, false),
            Id::CamHide => plain(ui, ph::EYE_SLASH, "Ocultar câmera para mim", false),
            Id::CamShow => plain(ui, ph::VIDEO_CAMERA, "Mostrar câmera", false),
            Id::ModServerMute => plain(ui, ph::MICROPHONE_SLASH, "Silenciar no servidor", false),
            Id::ModServerUnmute => plain(ui, ph::MICROPHONE, "Remover silêncio", false),
            Id::ModDisconnect => plain(ui, ph::PHONE_X, "Desconectar da voz", true),
            Id::ModStopStream => plain(ui, ph::STOP, "Encerrar transmissão", true),
            Id::ModRemovePhoto => plain(ui, ph::IMAGE, "Remover foto", false),
            Id::ModClearProfile => plain(ui, ph::TRASH, "Limpar perfil...", true),
            Id::Block => plain(ui, ph::PROHIBIT, "Bloquear", true),
            Id::Unblock => plain(ui, ph::CHECK, "Desbloquear", false),
            Id::MuteMic => plain(ui, ph::MICROPHONE, if f.my_muted { "Desmutar" } else { "Mutar" }, false),
            Id::Deafen => plain(ui, ph::HEADPHONES, if f.my_deafened { "Ouvir de novo" } else { "Ensurdecer" }, false),
            Id::CameraToggle => plain(ui, ph::VIDEO_CAMERA, if f.my_cam_on { "Desligar câmera" } else { "Ligar câmera" }, false),
            Id::CameraSettings => plain(ui, ph::GEAR_SIX, "Configurar câmera", false),
            Id::SwapScreen => plain(ui, ph::ARROWS_CLOCKWISE, if f.audio_only { "Trocar app" } else { "Trocar tela" }, false),
            Id::StopShare => plain(ui, ph::STOP, "Parar transmissão", true),
            Id::SelfPreviewFocus => plain(ui, ph::PUSH_PIN, if f.focused { "Desafixar prévia" } else { "Fixar prévia" }, false),
            Id::SelfPreviewToggle => plain(ui, ph::EYE, if f.preview_on { "Ocultar prévia" } else { "Mostrar prévia" }, false),
            Id::ModMove => {
                let others: Vec<(ChannelId, String)> = self
                    .channels
                    .iter()
                    .filter(|c| c.kind == ChannelKind::Voice && Some(c.id) != self.voice_members.get(&user.id).and_then(|m| m.channel_id))
                    .map(|c| (c.id, c.name.clone()))
                    .collect();
                ui_kit::menu_sub(ui, ph::ARROW_RIGHT, "Mover para", why, |ui| {
                    for (cid, name) in others {
                        if ui_kit::menu_row(ui, ph::SPEAKER_HIGH, &name, false).clicked() {
                            *act = Some(Act::MoveTo(cid));
                        }
                    }
                });
            }
            Id::ModSetRole => {
                let theirs = f.theirs;
                let roles = if f.mine.can_admin() && why.is_none() { f.mine.assignable() } else { Vec::new() };
                ui_kit::menu_sub(ui, ph::SHIELD, "Definir cargo", why, |ui| {
                    for r in roles {
                        if ui_kit::menu_choice(ui, theirs == r, role_label(r)).clicked() && r != theirs {
                            *act = Some(Act::SetRole(r));
                        }
                    }
                });
            }
            Id::Presence => {
                let current = self.my_presence();
                ui_kit::menu_sub(ui, ph::CIRCLE, "Status", None, |ui| {
                    for p in Presence::ALL {
                        // The presence's colour on its dot; the current one checked.
                        let icon = if current == p { ph::CHECK } else { ph::CIRCLE };
                        if ui_kit::menu_row_ex(ui, icon, Some(presence_color(p)), presence_label(p), false, false).on_hover_text(presence_hint(p)).clicked() {
                            *act = Some(Act::Presence(p));
                        }
                    }
                });
            }
            Id::VoiceFx => {
                let current = self.settings.voice_fx.effect;
                ui_kit::menu_sub(ui, ph::MASK_HAPPY, "Modificador de voz", None, |ui| {
                    for e in VoiceEffect::ALL {
                        if ui_kit::menu_choice(ui, current == e, e.label()).on_hover_text(e.hint()).clicked() {
                            *act = Some(Act::Effect(e));
                        }
                    }
                });
            }
        }
    }

    /// A slider row of the menu: a quiet label with its icon, then the 0-200% slider.
    fn menu_slider(ui: &mut egui::Ui, icon: &str, text: &str, value: &mut u32) -> egui::Response {
        ui.add_space(space::S);
        ui.horizontal(|ui| {
            ui.add_space(space::M);
            ui.label(RichText::new(format!("{icon}  {text}")).font(theme::body(12.5)).color(theme::NEVOA));
        });
        ui.horizontal(|ui| {
            ui.add_space(space::M);
            // (A fixed width: the menu sizes itself to its content.)
            ui.spacing_mut().slider_width = 168.0;
            ui.add(egui::Slider::new(value, 0..=200).suffix("%"))
        })
        .inner
    }

    /// A check row of the menu ("Silenciar para mim"), indented like the items.
    fn menu_check(ui: &mut egui::Ui, value: &mut bool, text: &str) -> egui::Response {
        ui.horizontal(|ui| {
            ui.add_space(space::M);
            ui.checkbox(value, text)
        })
        .inner
    }

    /// Volume slider (0-200%) and "Silenciar para mim" for somebody in my room.
    fn menu_voice_controls(&mut self, ui: &mut egui::Ui, user: &User, id: Id) {
        let before = self.settings.user_audio(&user.username);
        let mut pref = before;
        match id {
            Id::Volume => {
                let slider = Self::menu_slider(ui, ph::SPEAKER_HIGH, "Volume", &mut pref.volume_pct);
                if pref != before {
                    self.set_user_pref(user, pref, !slider.dragged());
                }
                if slider.drag_stopped() {
                    self.save_settings();
                }
                if pref.volume_pct != 100 {
                    let reset = ui.horizontal(|ui| {
                        ui.add_space(space::M);
                        ui_kit::button_sized(ui, Kind::Ghost, Size::Small, Some(ph::ARROW_COUNTER_CLOCKWISE), "Restaurar volume").clicked()
                    });
                    if reset.inner {
                        pref.volume_pct = 100;
                        self.set_user_pref(user, pref, true);
                    }
                }
            }
            _ => {
                if Self::menu_check(ui, &mut pref.muted, "Silenciar para mim").on_hover_text("Só você deixa de ouvir esta pessoa").changed() {
                    self.set_user_pref(user, pref, true);
                }
            }
        }
    }

    /// Their stream's volume and mute (separate from their microphone).
    fn menu_stream_controls(&mut self, ui: &mut egui::Ui, user: &User, id: Id) {
        let before = self.settings.user_audio(&user.username);
        let mut pref = before;
        match id {
            Id::StreamVolume => {
                let slider = Self::menu_slider(ui, ph::SPEAKER_LOW, "Volume da transmissão", &mut pref.stream_volume_pct);
                if pref != before {
                    self.set_user_pref(user, pref, !slider.dragged());
                }
                if slider.drag_stopped() {
                    self.save_settings();
                }
            }
            _ => {
                if Self::menu_check(ui, &mut pref.stream_muted, "Silenciar transmissão").changed() {
                    self.set_user_pref(user, pref, true);
                }
            }
        }
    }

    // ------------------------------------------------------------ actions

    /// Run what a click on the menu asked for. Public to the crate so the autotest can drive it.
    pub(crate) fn run_menu_act(&mut self, ctx: &egui::Context, user: &User, act: Act, pos: Pos2) {
        self.cmenu.actions += 1;
        let key = user.id.to_string();
        match act {
            Act::Item(id) => match id {
                Id::ViewProfile => self.open_profile_at(user, pos),
                Id::EditProfile => self.open_settings(),
                Id::SendMessage => self.open_dm(user.id),
                Id::Call => self.call_user(user.id),
                Id::CopyUsername => {
                    ctx.copy_text(user.username.clone());
                    self.toast(format!("Copiado: {}", user.username));
                }
                Id::StreamWatch => self.watch_focused(&key),
                Id::StreamStop => self.unwatch(&key),
                Id::StreamFocus => {
                    self.toggle_pin(crate::call::TileKey::Stream(user.id));
                    self.show_call_if_in_voice();
                }
                Id::StreamFullscreen => {
                    self.show_call_if_in_voice();
                    self.fullscreen_stream(ctx, &key);
                }
                Id::StreamPopOut => {
                    let popped = self.stream_popped(&key);
                    self.pop_out_stream(&key, !popped);
                }
                Id::CamFocus => {
                    self.toggle_pin(crate::call::TileKey::Member(user.id));
                    self.show_call_if_in_voice();
                }
                Id::CamHide => {
                    self.cmenu.hidden_cams.insert(user.id);
                    self.unpin(crate::call::TileKey::Member(user.id));
                    self.backend.send(Cmd::CameraHidden { identity: key, hidden: true });
                }
                Id::CamShow => {
                    self.cmenu.hidden_cams.remove(&user.id);
                    self.backend.send(Cmd::CameraHidden { identity: key, hidden: false });
                }
                Id::ModServerMute | Id::ModServerUnmute => {
                    self.admin(AdminCmd::Mute { user: user.id, muted: id == Id::ModServerMute });
                }
                Id::ModDisconnect => self.cmenu.confirm = Some(Confirm::Disconnect(user.clone())),
                Id::ModStopStream => self.admin(AdminCmd::StopStream { user: user.id }),
                Id::ModRemovePhoto => self.mod_remove_picture(user.id),
                Id::ModClearProfile => {
                    let (has_bio, has_status) = self.profile_clear_parts(user).unwrap_or((true, true));
                    let has_name = user.display_name.is_some();
                    let what = ModClearProfileRequest { bio: has_bio, status: has_status, display_name: has_name };
                    self.cmenu.confirm = Some(Confirm::Clear { user: user.clone(), what, has_bio, has_status, has_name });
                }
                Id::Block => self.block_user(user, true),
                Id::Unblock => self.block_user(user, false),
                Id::MuteMic => self.toggle_mute(),
                Id::Deafen => self.toggle_deafen(),
                Id::CameraToggle => self.toggle_camera(),
                Id::CameraSettings => self.open_camera_window(),
                Id::SwapScreen => self.open_golive_switch(),
                Id::StopShare => self.stop_share(),
                Id::SelfPreviewFocus => {
                    self.toggle_pin(crate::call::TileKey::Stream(user.id));
                    self.show_call_if_in_voice();
                }
                Id::SelfPreviewToggle => {
                    let on = self.settings.show_self_preview;
                    self.set_self_preview(!on);
                }
                // Controls that act where they are drawn (sliders, toggles, submenus).
                Id::Volume | Id::MuteLocal | Id::StreamVolume | Id::StreamMute | Id::ModMove | Id::ModSetRole | Id::Presence | Id::VoiceFx => {}
            },
            Act::MoveTo(channel) => self.admin(AdminCmd::Move { user: user.id, channel }),
            Act::SetRole(role) => {
                if role != self.role_of(user.id) {
                    self.cmenu.confirm = Some(Confirm::SetRole { user: user.clone(), role });
                }
            }
            Act::Presence(p) => self.set_presence(p),
            Act::Effect(e) => {
                let mut cfg = self.settings.voice_fx;
                cfg.effect = e;
                self.set_voice_fx(cfg, true);
            }
        }
    }

    fn show_call_if_in_voice(&mut self) {
        if self.voice.channel.is_some() {
            self.center = Center::Call;
        }
    }

    // ------------------------------------------------------------ confirmation

    /// The confirmation window (disconnect, role change, clearing a profile).
    pub fn menu_confirm_window(&mut self, ctx: &egui::Context) {
        let Some(mut confirm) = self.cmenu.confirm.clone() else { return };
        let (title, yes, body, hint, kind): (&str, &str, String, &str, Kind) = match &confirm {
            Confirm::Disconnect(u) => (
                "Desconectar da voz",
                "Desconectar",
                format!("Desconectar {} do canal de voz?", u.shown_name()),
                "A pessoa sai da sala e pode entrar de novo. Fica no registro de moderação.",
                Kind::Danger,
            ),
            Confirm::SetRole { user, role } => (
                "Mudar cargo",
                "Mudar cargo",
                format!("Mudar o cargo de {} para {}?", user.shown_name(), role_label(*role)),
                "Fica no registro de moderação.",
                if matches!(role, Role::Admin | Role::Owner) { Kind::Danger } else { Kind::Primary },
            ),
            Confirm::Clear { user, .. } => (
                "Limpar perfil",
                "Limpar",
                format!("Limpar partes do perfil de {}?", user.shown_name()),
                "Fica no registro de moderação.",
                Kind::Danger,
            ),
        };
        let empty = matches!(&confirm, Confirm::Clear { what, .. } if what.is_empty());
        let answer = ui_kit::confirm(ctx, "menu_confirm", title, yes, kind, !empty, |ui| {
            ui_kit::body_text(ui, body);
            if let Confirm::Clear { what, has_bio, has_status, has_name, .. } = &mut confirm {
                ui.add_space(space::S);
                ui.add_enabled(*has_bio, egui::Checkbox::new(&mut what.bio, "Sobre mim"));
                ui.add_enabled(*has_status, egui::Checkbox::new(&mut what.status, "Status"));
                ui.add_enabled(*has_name, egui::Checkbox::new(&mut what.display_name, "Nome de exibição"));
                ui.add_space(space::S);
            }
            ui_kit::hint(ui, hint);
        });
        match answer {
            Some(true) => self.menu_confirm_accept(Some(confirm)),
            Some(false) => self.cmenu.confirm = None,
            None => self.cmenu.confirm = Some(confirm),
        }
    }

    /// "Confirmar" (also called by the autotest).
    pub(crate) fn menu_confirm_accept(&mut self, edited: Option<Confirm>) {
        let Some(c) = edited.or_else(|| self.cmenu.confirm.clone()) else { return };
        self.cmenu.confirm = None;
        match c {
            Confirm::Disconnect(u) => self.admin(AdminCmd::Disconnect { user: u.id }),
            Confirm::SetRole { user, role } => self.admin(AdminCmd::SetRole { user: user.id, role }),
            Confirm::Clear { user, what, .. } => {
                if !what.is_empty() {
                    self.backend.send(Cmd::Profile(ProfileCmd::ModClear { user: user.id, what }));
                }
            }
        }
    }

    /// Is this camera hidden for me? (The call view draws such a person as a plain tile.)
    pub fn camera_hidden_for_me(&self, id: UserId) -> bool {
        self.cmenu.hidden_cams.contains(&id)
    }
}

// ---------------------------------------------------------------------------------------------
// Tests: which items appear for which state (no egui)
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Another member, online, no voice, DMs and profiles supported, I am a plain member.
    fn base() -> Facts {
        Facts { hub_up: true, profile_ok: true, dms_ok: true, roles_ok: true, mine: Role::Member, theirs: Role::Member, ..Default::default() }
    }

    fn has(f: &Facts, id: Id) -> bool {
        ids(&rows(f)).contains(&id)
    }

    fn mods() -> [Id; 8] {
        [Id::ModServerMute, Id::ModServerUnmute, Id::ModDisconnect, Id::ModMove, Id::ModStopStream, Id::ModSetRole, Id::ModRemovePhoto, Id::ModClearProfile]
    }

    #[test]
    fn an_offline_member_gets_the_basics_only() {
        let f = base();
        assert_eq!(ids(&rows(&f)), vec![Id::ViewProfile, Id::SendMessage, Id::Call, Id::CopyUsername, Id::Block]);
        // Sections are separated.
        assert_eq!(rows(&f).iter().filter(|r| **r == Row::Sep).count(), 1);
    }

    #[test]
    fn voice_controls_need_the_same_room() {
        let mut f = base();
        f.in_voice = true;
        assert!(!has(&f, Id::Volume) && !has(&f, Id::MuteLocal), "another room: no volume");
        f.same_room = true;
        assert!(has(&f, Id::Volume) && has(&f, Id::MuteLocal));
        let r = rows(&f);
        let at = |id| ids(&r).iter().position(|i| *i == id).unwrap();
        assert!(at(Id::CopyUsername) < at(Id::Volume) && at(Id::Volume) < at(Id::Block));
    }

    #[test]
    fn a_live_person_in_my_room_has_the_stream_section() {
        let mut f = Facts { in_voice: true, same_room: true, live: true, ..base() };
        assert!(has(&f, Id::StreamWatch) && !has(&f, Id::StreamStop));
        assert_eq!(disabled_reason(&rows(&f), Id::StreamWatch), None);
        // Watching: stop, volume, mute, enlarge, fullscreen, window.
        f.watching = true;
        let r = rows(&f);
        for id in [Id::StreamStop, Id::StreamVolume, Id::StreamMute, Id::StreamFocus, Id::StreamFullscreen, Id::StreamPopOut] {
            assert!(ids(&r).contains(&id), "{id:?}");
        }
        assert!(!ids(&r).contains(&Id::StreamWatch));
        // Fullscreen is not offered while it is in its own window.
        f.popped = true;
        assert!(disabled_reason(&rows(&f), Id::StreamFullscreen).is_some());
        // Audio only: no picture to enlarge.
        let a = Facts { audio_only: true, popped: false, ..f.clone() };
        let r = rows(&a);
        assert!(ids(&r).contains(&Id::StreamStop) && ids(&r).contains(&Id::StreamVolume));
        assert!(!ids(&r).contains(&Id::StreamFocus) && !ids(&r).contains(&Id::StreamFullscreen) && !ids(&r).contains(&Id::StreamPopOut));
    }

    #[test]
    fn a_live_person_in_another_room_can_not_be_watched() {
        let f = Facts { in_voice: true, live: true, ..base() };
        let r = rows(&f);
        assert!(ids(&r).contains(&Id::StreamWatch));
        assert!(disabled_reason(&r, Id::StreamWatch).unwrap().contains("canal de voz"));
        assert!(!ids(&r).contains(&Id::Volume));
    }

    #[test]
    fn a_stream_tile_leads_with_the_stream_and_a_camera_tile_with_the_camera() {
        let f = Facts { in_voice: true, same_room: true, live: true, camera_on: true, lead: Lead::Stream, ..base() };
        let r = rows(&f);
        assert_eq!(r[0], Row::Label("Transmissão"));
        assert_eq!(ids(&r)[0], Id::StreamWatch);
        let f = Facts { lead: Lead::Camera, ..f };
        assert_eq!(rows(&f)[0], Row::Label("Câmera"));
        assert_eq!(ids(&rows(&f))[0], Id::CamFocus);
        // Same items either way, only the order changes.
        let mut a = ids(&rows(&Facts { lead: Lead::Stream, ..f.clone() }));
        let mut b = ids(&rows(&Facts { lead: Lead::None, ..f }));
        a.sort_by_key(|i| format!("{i:?}"));
        b.sort_by_key(|i| format!("{i:?}"));
        assert_eq!(a, b);
    }

    #[test]
    fn camera_items_follow_camera_and_room_and_hiding() {
        let mut f = Facts { in_voice: true, camera_on: true, ..base() };
        assert!(!has(&f, Id::CamFocus), "the video only exists in my room");
        f.same_room = true;
        assert!(has(&f, Id::CamFocus) && has(&f, Id::CamHide) && !has(&f, Id::CamShow));
        f.camera_hidden = true;
        assert!(!has(&f, Id::CamFocus) && !has(&f, Id::CamHide) && has(&f, Id::CamShow));
        f.camera_on = false;
        assert!(!has(&f, Id::CamShow));
    }

    #[test]
    fn members_see_no_moderation_only_a_hint_on_streams() {
        let f = Facts { in_voice: true, same_room: true, live: true, has_avatar: true, clearable: true, ..base() };
        for m in mods() {
            if m != Id::ModStopStream {
                assert!(!has(&f, m), "{m:?}");
            }
        }
        // The one thing shown to a member is disabled and says who may do it.
        let r = rows(&f);
        assert!(!enabled_ids(&r).contains(&Id::ModStopStream));
        assert_eq!(disabled_reason(&r, Id::ModStopStream), Some(NEED_MOD));
        // Nothing live: nothing at all.
        let quiet = Facts { live: false, ..f };
        assert!(!has(&quiet, Id::ModStopStream));
    }

    #[test]
    fn moderator_acts_on_members_but_cannot_set_roles() {
        let f = Facts { mine: Role::Moderator, in_voice: true, other_channels: true, live: true, has_avatar: true, clearable: true, ..base() };
        let r = rows(&f);
        for id in [Id::ModServerMute, Id::ModDisconnect, Id::ModMove, Id::ModStopStream, Id::ModRemovePhoto, Id::ModClearProfile] {
            assert!(enabled_ids(&r).contains(&id), "{id:?}");
        }
        assert_eq!(disabled_reason(&r, Id::ModSetRole), Some(NEED_ADMIN));
        // Server-muted: the item flips.
        let f2 = Facts { server_muted: true, ..f };
        let r = rows(&f2);
        assert!(ids(&r).contains(&Id::ModServerUnmute) && !ids(&r).contains(&Id::ModServerMute));
    }

    #[test]
    fn moderators_can_not_act_on_equals_or_above() {
        for theirs in [Role::Moderator, Role::Admin, Role::Owner] {
            let f = Facts { mine: Role::Moderator, theirs, in_voice: true, has_avatar: true, clearable: true, ..base() };
            let r = rows(&f);
            assert_eq!(disabled_reason(&r, Id::ModDisconnect), Some(OUTRANKED), "{theirs:?}");
            assert_eq!(disabled_reason(&r, Id::ModServerMute), Some(OUTRANKED));
            assert!(!ids(&r).contains(&Id::ModRemovePhoto) && !ids(&r).contains(&Id::ModClearProfile));
            assert!(!enabled_ids(&r).contains(&Id::ModSetRole));
        }
    }

    #[test]
    fn admin_sets_roles_below_and_owner_everyone_but_himself() {
        let admin = Facts { mine: Role::Admin, ..base() };
        assert!(enabled_ids(&rows(&Facts { theirs: Role::Member, ..admin.clone() })).contains(&Id::ModSetRole));
        assert!(enabled_ids(&rows(&Facts { theirs: Role::Moderator, ..admin.clone() })).contains(&Id::ModSetRole));
        assert!(!enabled_ids(&rows(&Facts { theirs: Role::Admin, ..admin.clone() })).contains(&Id::ModSetRole));
        assert!(!enabled_ids(&rows(&Facts { theirs: Role::Owner, ..admin })).contains(&Id::ModSetRole));
        let owner = Facts { mine: Role::Owner, ..base() };
        for theirs in [Role::Member, Role::Moderator, Role::Admin] {
            assert!(enabled_ids(&rows(&Facts { theirs, ..owner.clone() })).contains(&Id::ModSetRole), "{theirs:?}");
        }
    }

    #[test]
    fn nothing_to_clear_is_explained() {
        let f = Facts { mine: Role::Admin, clearable: false, ..base() };
        assert!(disabled_reason(&rows(&f), Id::ModClearProfile).is_some());
        // No profile support: no clear at all.
        let f = Facts { profile_ok: false, ..f };
        assert!(!has(&f, Id::ModClearProfile));
    }

    #[test]
    fn moderation_off_when_the_hub_has_no_roles() {
        let f = Facts { roles_ok: false, mine: Role::Owner, in_voice: true, live: true, ..base() };
        for m in mods() {
            assert!(!has(&f, m));
        }
    }

    #[test]
    fn block_and_call_follow_the_block_state() {
        let f = base();
        assert!(has(&f, Id::Block) && !has(&f, Id::Unblock) && enabled_ids(&rows(&f)).contains(&Id::Call));
        let b = Facts { blocked: true, ..f.clone() };
        assert!(has(&b, Id::Unblock) && !has(&b, Id::Block));
        assert!(disabled_reason(&rows(&b), Id::Call).is_some());
        // Offline: no call either.
        let o = Facts { hub_up: false, ..f.clone() };
        assert_eq!(disabled_reason(&rows(&o), Id::Call), Some(OFFLINE));
        // Already in a call with them: no "Ligar".
        let c = Facts { in_call_with: true, ..f.clone() };
        assert!(!has(&c, Id::Call));
        // A hub without DMs: no message, call or block.
        let n = Facts { dms_ok: false, ..f };
        assert_eq!(ids(&rows(&n)), vec![Id::ViewProfile, Id::CopyUsername]);
    }

    #[test]
    fn the_profile_card_menu_does_not_offer_ver_perfil() {
        let f = Facts { origin: Origin::Popout, ..base() };
        assert!(!has(&f, Id::ViewProfile) && has(&f, Id::SendMessage));
        let me = Facts { origin: Origin::Popout, me: true, ..base() };
        assert!(!has(&me, Id::ViewProfile) && has(&me, Id::EditProfile));
    }

    fn mine() -> Facts {
        Facts { me: true, dms_ok: false, presence_ok: true, ..base() }
    }

    #[test]
    fn my_own_entry_outside_voice() {
        let f = mine();
        assert_eq!(ids(&rows(&f)), vec![Id::ViewProfile, Id::EditProfile, Id::CopyUsername, Id::Presence]);
        // Never the things you do to others.
        for id in [Id::SendMessage, Id::Call, Id::Block, Id::Volume, Id::MuteLocal, Id::StreamWatch] {
            assert!(!has(&f, id));
        }
        // An older hub: no status switch, no "Editar perfil".
        let old = Facts { profile_ok: false, presence_ok: false, ..f };
        assert_eq!(ids(&rows(&old)), vec![Id::ViewProfile, Id::CopyUsername]);
    }

    #[test]
    fn my_own_entry_in_voice_and_live() {
        let mut f = Facts { my_in_voice: true, ..mine() };
        let r = rows(&f);
        for id in [Id::MuteMic, Id::Deafen, Id::CameraToggle, Id::CameraSettings, Id::VoiceFx] {
            assert!(enabled_ids(&r).contains(&id), "{id:?}");
        }
        // "Configurar câmera" right after the camera switch (my camera tile's menu too).
        let ids_now = ids(&r);
        let at = ids_now.iter().position(|i| *i == Id::CameraToggle).unwrap();
        assert_eq!(ids_now.get(at + 1), Some(&Id::CameraSettings));
        assert!(!ids(&r).contains(&Id::StopShare));
        // Mute explains itself.
        f.my_server_muted = true;
        assert!(disabled_reason(&rows(&f), Id::MuteMic).unwrap().contains("moderador"));
        f.my_server_muted = false;
        f.my_ptt = true;
        assert!(disabled_reason(&rows(&f), Id::MuteMic).unwrap().contains("Push-to-talk"));
        f.my_ptt = false;
        f.my_no_mic = true;
        assert_eq!(disabled_reason(&rows(&f), Id::MuteMic), Some("Sem microfone"));
        assert!(!has(&f, Id::VoiceFx), "no microphone: no voice changer");
        // A private call has no voice changer either.
        let call = Facts { my_in_dm_call: true, ..Facts { my_in_voice: true, ..mine() } };
        assert!(!has(&call, Id::VoiceFx) && has(&call, Id::MuteMic));
        // Live with a preview.
        let live = Facts { live: true, preview_on: true, lead: Lead::Stream, ..Facts { my_in_voice: true, ..mine() } };
        let r = rows(&live);
        assert_eq!(ids(&r)[0], Id::SwapScreen, "a stream tile leads with the stream");
        for id in [Id::StopShare, Id::SelfPreviewFocus, Id::SelfPreviewToggle] {
            assert!(enabled_ids(&r).contains(&id), "{id:?}");
        }
        // Preview off: nothing to enlarge, but it can come back.
        let off = Facts { preview_on: false, ..live.clone() };
        assert!(!has(&off, Id::SelfPreviewFocus) && has(&off, Id::SelfPreviewToggle));
        // Audio only: no preview at all.
        let audio = Facts { audio_only: true, ..live.clone() };
        assert!(!has(&audio, Id::SelfPreviewFocus) && !has(&audio, Id::SelfPreviewToggle) && has(&audio, Id::StopShare));
        // Switching: "Trocar tela" waits.
        let sw = Facts { switching: true, ..live };
        assert_eq!(disabled_reason(&rows(&sw), Id::SwapScreen), Some("Trocando..."));
    }

    #[test]
    fn the_same_person_gets_the_same_items_from_any_origin() {
        let f = Facts { in_voice: true, same_room: true, live: true, camera_on: true, mine: Role::Admin, theirs: Role::Member, other_channels: true, has_avatar: true, clearable: true, ..base() };
        let reference = {
            let mut v = ids(&rows(&f));
            v.sort_by_key(|i| format!("{i:?}"));
            v
        };
        for origin in [Origin::Sidebar, Origin::Online, Origin::Tile, Origin::Dm, Origin::Chat] {
            for lead in [Lead::None, Lead::Stream, Lead::Camera] {
                let mut v = ids(&rows(&Facts { origin, lead, ..f.clone() }));
                v.sort_by_key(|i| format!("{i:?}"));
                assert_eq!(v, reference, "{origin:?} / {lead:?}");
            }
        }
    }

    #[test]
    fn a_card_menu_has_one_pin_item() {
        // B is live with the camera on and in my room; the menu opens on one of B's cards, whose
        // own "Fixar" / "Desafixar" comes first: the person's pin items are left out.
        let f = Facts { in_voice: true, same_room: true, live: true, watching: true, camera_on: true, focused: true, ..base() };
        let full = rows(&f);
        assert!(ids(&full).contains(&Id::StreamFocus) && ids(&full).contains(&Id::CamFocus));
        let card = card_rows(&full);
        for id in PIN_ITEMS {
            assert!(!ids(&card).contains(&id), "{id:?}");
        }
        // Everything else stays, in order, with no separator doubled or dangling.
        let rest: Vec<Id> = ids(&full).into_iter().filter(|i| !PIN_ITEMS.contains(i)).collect();
        assert_eq!(ids(&card), rest);
        assert!(!matches!(card.first(), Some(Row::Sep)) && !matches!(card.last(), Some(Row::Sep)));
        assert!(card.windows(2).all(|w| !(w[0] == Row::Sep && w[1] == Row::Sep)));
        // My own stream card: "Fixar prévia" goes too.
        let me = Facts { me: true, dms_ok: false, my_in_voice: true, live: true, preview_on: true, lead: Lead::Stream, ..base() };
        assert!(ids(&rows(&me)).contains(&Id::SelfPreviewFocus) && !ids(&card_rows(&rows(&me))).contains(&Id::SelfPreviewFocus));
    }

    #[test]
    fn no_double_separators() {
        let f = Facts { in_voice: true, same_room: true, live: true, camera_on: true, mine: Role::Owner, other_channels: true, has_avatar: true, clearable: true, ..base() };
        let r = rows(&f);
        assert!(!matches!(r.first(), Some(Row::Sep)) && !matches!(r.last(), Some(Row::Sep)));
        assert!(r.windows(2).all(|w| !(w[0] == Row::Sep && w[1] == Row::Sep)));
    }
}
