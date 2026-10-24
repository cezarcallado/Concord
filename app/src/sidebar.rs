//! The v1.0 sidebar (docs/PLANO-v1.0.md, the mockup in dist/design/voxhub-v1-proposta.html):
//!
//! - the server header (its name in the display font, "N online · M na voz"); a click opens the
//!   server menu: server settings, invites, my hosted server, claiming an ownerless server,
//!   "Configurações" and "Trocar de servidor";
//! - DMs, text channels (🔒 when private, the unread counter and the "@"), voice rooms with the
//!   people inside (the lamp ring on whoever speaks, "ao vivo" (a click watches), the camera);
//! - the voice panel while in (or joining) a call: the state, the room (a click opens the stage),
//!   camera, screen share and leave;
//! - the "você" panel: my picture, name and status (a click changes the presence), the
//!   microphone, deafen and settings buttons.
//!
//! Only what is open in the center is highlighted (`layout::open_in_center`). Rows are painted
//! (no nested layouts per row); the member list on the right of the chat is here too.
//! A child module of `app`.

use eframe::egui::{self, Align2, Color32, Margin, Pos2, Rect, RichText, Sense, Vec2};
use vox_proto::{ChannelKind, User, VoiceMember};

use super::{channel_label, App, CameraUi, Share, ICON_PRIVATE};
use crate::context_menu::{Lead, Origin};
use crate::layout::Open;
use crate::theme::{self, ph, radius};
use crate::ui_kit::{self, IconState, Kind, Size};

/// The bottom panels' background (a shade under the sidebar).
const PANEL_BG: Color32 = Color32::from_rgb(0x17, 0x22, 0x2D);
/// A person in a voice room.
const MEMBER_H: f32 = 28.0;
const MEMBER_INDENT: f32 = 30.0;
const MEMBER_AVATAR: f32 = 22.0;
/// A row of the member list.
const ONLINE_H: f32 = 40.0;
const ONLINE_AVATAR: f32 = 30.0;

/// Paint a small icon at the right edge `x` (moving left), returning its rect.
fn icon_at(painter: &egui::Painter, x: &mut f32, cy: f32, icon: &str, color: Color32) -> Rect {
    let g = painter.layout_no_wrap(icon.to_owned(), theme::body(14.0), color);
    let r = Rect::from_min_size(Pos2::new(*x - g.size().x, cy - g.size().y / 2.0), g.size());
    painter.galley(r.min, g, color);
    *x = r.left() - 5.0;
    r
}

impl App {
    pub(super) fn left_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("server_head").resizable(false)
            .frame(egui::Frame::new().inner_margin(Margin { left: 8, right: 8, top: 8, bottom: 8 }))
            .show(ui, |ui| self.server_header(ui));
        egui::Panel::bottom("me_panel").resizable(false).frame(egui::Frame::new().fill(PANEL_BG).inner_margin(Margin::symmetric(8, 8))).show(ui, |ui| self.me_panel(ui));
        if self.voice.channel.is_some() || self.voice.joining.is_some() || self.voice.error.is_some() {
            egui::Panel::bottom("voice_panel").resizable(false)
                .frame(egui::Frame::new().fill(PANEL_BG).inner_margin(Margin { left: 12, right: 8, top: 10, bottom: 8 }))
                .show(ui, |ui| self.voice_panel(ui));
        }
        egui::CentralPanel::no_frame().show(ui, |ui| {
            // v1.1: the list scrolls at any window height. Its content goes in a frame (the side
            // padding), never a `new_child`: a child does not report its size to the scroll area,
            // which then thought the list was empty and never scrolled.
            let probe = !self.persist;
            let out = egui::ScrollArea::vertical().id_salt("sidebar_lists").auto_shrink([false, false]).show(ui, |ui| {
                if probe {
                    ui_kit::apply_scroll_request(ui, ui_kit::SIDEBAR_SCROLL);
                }
                egui::Frame::new().inner_margin(Margin::symmetric(8, 0)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.add_space(8.0);
                    self.channel_lists(ui);
                    ui.add_space(12.0);
                });
            });
            if probe {
                // Autotests: what the list scrolls (`autotest_uix.rs`).
                ui_kit::note_scroll(ui.ctx(), ui_kit::SIDEBAR_SCROLL, &out);
            }
        });
    }

    /// A section header of the list, in sentence case.
    fn list_section(ui: &mut egui::Ui, text: &str) {
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui_kit::section(ui, text);
        });
        ui.add_space(2.0);
    }

    fn channel_lists(&mut self, ui: &mut egui::Ui) {
        self.sidebar_rows.clear();
        let open = self.open_in_center();
        self.dm_sidebar(ui);
        Self::list_section(ui, "Canais de texto");
        let texts: Vec<vox_proto::Channel> = self.channels.iter().filter(|c| c.kind == ChannelKind::Text).cloned().collect();
        for c in texts {
            let unread = self.unread.get(&c.id).copied().unwrap_or(0);
            let mentions = self.chatx.mention_unread.get(&c.id).copied().unwrap_or(0);
            let hover = c.private.is_some().then_some(super::PRIVATE_HOVER);
            let label = channel_label(&c, "");
            let icon = if c.private.is_some() { ICON_PRIVATE } else { ph::HASH };
            // The name on the left (cut with "…"), the counter and the "@" in their own space.
            let row = ui_kit::Row { icon, name: &c.name, label: &label, selected: open == Open::Text(c.id), unread: unread > 0, count: unread, mentions, hover };
            let (resp, drawn) = ui_kit::sidebar_row(ui, row);
            self.sidebar_rows.push(drawn);
            if resp.clicked() {
                self.select_text(c.id);
            }
        }
        Self::list_section(ui, "Canais de voz");
        let voices: Vec<vox_proto::Channel> = self.channels.iter().filter(|c| c.kind == ChannelKind::Voice).cloned().collect();
        for c in voices {
            let here = self.voice.channel == Some(c.id);
            let joining = self.voice.joining == Some(c.id);
            let suffix = if joining { "  (entrando...)" } else { "" };
            let hover = match (here, c.private.is_some()) {
                (true, _) => "Clique para abrir a sala",
                (false, true) => "Canal privado: só quem tem acesso vê e entra. Clique para entrar",
                (false, false) => "Clique para entrar",
            };
            let label = channel_label(&c, suffix);
            // (A private room: the lock before its name, as in `channel_label`.)
            let name = if c.private.is_some() || !suffix.is_empty() { label.split_once(' ').map_or(label.as_str(), |(_, n)| n) } else { c.name.as_str() };
            let row = ui_kit::Row { icon: ph::SPEAKER_HIGH, name, label: &label, selected: open == Open::Stage(c.id), unread: false, count: 0, mentions: 0, hover: Some(hover) };
            let (resp, drawn) = ui_kit::sidebar_row(ui, row);
            self.sidebar_rows.push(drawn);
            if resp.clicked() {
                self.join_voice(c.id);
            }
            let members: Vec<VoiceMember> = self.voice_members.values().filter(|m| m.channel_id == Some(c.id)).cloned().collect();
            for m in members {
                self.member_row(ui, &m, here);
            }
            if !self.voice_members.values().any(|m| m.channel_id == Some(c.id)) {
                ui.add_space(2.0);
            }
        }
    }

    // ------------------------------------------------------------ server header and menu

    fn server_header(&mut self, ui: &mut egui::Ui) {
        let in_voice = self.voice_members.values().filter(|m| m.channel_id.is_some()).count();
        let mut sub = format!("{} online · {in_voice} na voz", self.online.len());
        let no_owner = self.roles.supported && self.roles.account.is_some_and(|a| a.no_owner);
        if no_owner {
            sub.push_str(" · sem dono");
        }
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 50.0), Sense::click());
        // v1.1 (T0): the connection's lock, before the online count ("Conexão protegida" / "Conexão
        // sem proteção (servidor antigo)" under the pointer).
        let lock = self.tls_lock();
        let mut lock_rect = None;
        if ui.is_rect_visible(rect) {
            let p = ui.painter();
            if resp.hovered() {
                p.rect_filled(rect, radius::ROW, theme::ROW_HOVER);
            }
            let x = rect.left() + 8.0;
            let caret = p.layout_no_wrap(ph::CARET_DOWN.to_owned(), theme::body(15.0), theme::NEVOA);
            let name = crate::app::notify::fit_name(p, &self.server_name, theme::display(theme::size::HEADING), theme::GIZ, rect.width() - 24.0 - caret.size().x);
            let name_h = name.size().y;
            p.galley(Pos2::new(x, rect.top() + 6.0), name, theme::GIZ);
            p.galley(Pos2::new(rect.right() - 8.0 - caret.size().x, rect.top() + 6.0 + (name_h - caret.size().y) / 2.0), caret, theme::NEVOA);
            let mut sub_x = x;
            if let Some((icon, _, color)) = lock {
                let g = p.layout_no_wrap(icon.to_owned(), theme::body(13.0), color);
                let r = egui::Rect::from_min_size(Pos2::new(x, rect.bottom() - 7.0 - g.size().y), g.size());
                p.galley(r.min, g, color);
                lock_rect = Some(r.expand(3.0));
                sub_x = r.right() + 4.0;
            }
            p.text(Pos2::new(sub_x, rect.bottom() - 7.0), Align2::LEFT_BOTTOM, &sub, theme::body(12.0), if no_owner { theme::AVISO } else { theme::NEVOA });
        }
        let over_lock = lock_rect.zip(resp.hover_pos()).is_some_and(|(r, pos)| r.contains(pos));
        let resp = resp.on_hover_text(match lock {
            Some((_, text, _)) if over_lock => text,
            _ => "Menu do servidor",
        });
        egui::Popup::menu(&resp).frame(ui_kit::popup_frame(ui.style())).show(|ui| self.server_header_menu(ui));
    }

    /// Everything that is about the server (and leaving it): the old header buttons live here.
    fn server_header_menu(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(230.0);
        let admin = self.roles.supported && self.my_role().can_admin();
        let mut any = false;
        if admin {
            any = true;
            if ui_kit::menu_row(ui, ph::WRENCH, "Configurações do servidor", false).on_hover_text("Membros, cargos, convites e o registro de moderação").clicked() {
                self.open_server_settings();
                ui.close();
            }
        }
        if self.host.runner.is_some() {
            any = true;
            if ui_kit::menu_row(ui, ph::HARD_DRIVES, "Meu servidor", false).on_hover_text("Status, portas e convites do servidor que você hospeda").clicked() {
                self.host.panel_open = true;
                ui.close();
            }
        } else if admin {
            any = true;
            if ui_kit::menu_row(ui, ph::ENVELOPE_SIMPLE, "Convidar", false).on_hover_text("Criar um código de convite para este servidor").clicked() {
                self.open_server_settings();
                self.roles.tab = crate::roles::ServerTab::Invites;
                ui.close();
            }
        }
        if self.roles.supported && self.roles.account.is_some_and(|a| a.no_owner) {
            any = true;
            if ui_kit::menu_row(ui, ph::CROWN, "Reivindicar servidor (sem dono)", false).on_hover_text("Este servidor ainda não tem dono. Clique para reivindicar.").clicked() {
                self.roles.claim_open = true;
                self.roles.claim_msg = None;
                ui.close();
            }
        }
        if any {
            ui_kit::menu_separator(ui);
        }
        // v1.1 (T0): the lock and the server's code, for everybody.
        if ui_kit::menu_row(ui, ph::SHIELD, crate::app::tls_ui::SECURITY_TITLE, false).on_hover_text("Se a conexão com este servidor é protegida, e o código dele").clicked() {
            self.tls.security_open = true;
            ui.close();
        }
        if ui_kit::menu_row(ui, ph::GEAR_SIX, "Configurações", false).clicked() {
            self.open_settings();
            ui.close();
        }
        let hover = if self.host.hosting() {
            "Sair deste servidor e voltar para a lista de servidores. Seu servidor continua ligado."
        } else {
            "Sair deste servidor e voltar para a lista de servidores"
        };
        if ui_kit::menu_row(ui, ph::ARROWS_LEFT_RIGHT, "Trocar de servidor", false).on_hover_text(hover).clicked() {
            ui.close();
            self.switch_server();
        }
    }

    // ------------------------------------------------------------ voice rooms

    /// One person in a voice room: the picture (with the lamp ring while they speak), the name,
    /// then on the right "ao vivo" (a click watches / stops), the camera, deafened, muted, my
    /// local volume / mute of them, and who watches their live.
    fn member_row(&mut self, ui: &mut egui::Ui, m: &VoiceMember, same_channel: bool) {
        let me = self.my_id() == Some(m.user.id);
        // My own ring follows what is actually sent (gate open), not the room's report. Muted
        // still rings while a soundboard sound of theirs plays (a muted mic never puts anyone in
        // the speaking set); server-muted never does (the hub refuses their sounds).
        let speaking = if me { self.sending_voice() } else { self.voice.speaking.contains(&m.user.id.to_string()) } && !m.server_muted;
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), MEMBER_H), Sense::click());
        if !ui.is_rect_visible(rect) {
            return;
        }
        let key = m.user.id.to_string();
        let audio_only = m.streaming && self.stream_is_audio_only(m);
        let watching = m.streaming && !me && self.is_watching(&key);
        let ctx = ui.ctx().clone();
        let painter = ui.painter_at(rect);
        if resp.hovered() {
            painter.rect_filled(rect, radius::ROW, theme::ROW_HOVER);
        }
        let cy = rect.center().y;
        let center = Pos2::new(rect.left() + MEMBER_INDENT + MEMBER_AVATAR / 2.0, cy);
        crate::avatars::draw_avatar(&mut self.avatars.store, &painter, &ctx, center, MEMBER_AVATAR / 2.0, &m.user, crate::avatars::Surface::Sidebar);
        if speaking {
            // The ring reaches past the row's top and bottom (it is wider than the row is tall):
            // painted unclipped by the row, into the gap between rows, so it is never cut and the
            // rows do not move when it comes and goes. Still clipped by the list's scroll area.
            let ring = ui.painter().with_clip_rect(ui_kit::speaking_ring_rect(center, MEMBER_AVATAR / 2.0).union(rect).intersect(ui.clip_rect()));
            ui_kit::paint_speaking_ring(&ring, center, MEMBER_AVATAR / 2.0, if resp.hovered() { theme::ROW_HOVER } else { theme::PAREDE });
        }
        // Right to left: the live tag, then the small icons.
        let mut x = rect.right() - 6.0;
        let mut live_rect = None;
        if m.streaming {
            let s = ui_kit::live_tag_size(&painter);
            let r = Rect::from_min_size(Pos2::new(x - s.x, cy - s.y / 2.0), s);
            ui_kit::paint_live_tag(&painter, r);
            live_rect = Some(r);
            x = r.left() - 5.0;
            if audio_only {
                icon_at(&painter, &mut x, cy, super::AUDIO_ONLY_ICON, theme::NEVOA);
            }
        }
        let mut viewers_rect = None;
        if m.streaming && self.viewers_supported {
            let n = self.viewers_of(m.user.id).len();
            let g = painter.layout_no_wrap(format!("{} {n}", ph::EYE), theme::body(12.0), theme::NEVOA);
            let r = Rect::from_min_size(Pos2::new(x - g.size().x, cy - g.size().y / 2.0), g.size());
            painter.galley(r.min, g, theme::NEVOA);
            viewers_rect = Some(r);
            x = r.left() - 6.0;
        }
        if self.camera_flag_of(m) {
            icon_at(&painter, &mut x, cy, ph::VIDEO_CAMERA, theme::NEVOA);
        }
        if m.deafened {
            icon_at(&painter, &mut x, cy, super::ICON_DEAF, theme::AO_VIVO);
        }
        if m.server_muted {
            icon_at(&painter, &mut x, cy, super::ICON_MUTED, theme::AO_VIVO);
        } else if m.muted {
            icon_at(&painter, &mut x, cy, super::ICON_MUTED, theme::NEVOA);
        }
        let mut local_rect = None;
        if !me && same_channel {
            let pref = self.settings.user_audio(&m.user.username);
            if pref.muted {
                local_rect = Some((icon_at(&painter, &mut x, cy, super::ICON_SPEAKER_OFF, theme::GIZ), "Silenciado só para você".to_string()));
            } else if pref.volume_pct != 100 {
                local_rect = Some((icon_at(&painter, &mut x, cy, super::ICON_VOL, theme::GIZ), format!("Volume ajustado só para você: {}%", pref.volume_pct)));
            }
        }
        let role = if self.roles.supported { m.user.role } else { Default::default() };
        let role_rect = match (crate::roles::role_icon(role), crate::roles::role_color(role)) {
            (Some(icon), Some(color)) => Some(icon_at(&painter, &mut x, cy, icon, color)),
            _ => None,
        };
        let name_left = center.x + MEMBER_AVATAR / 2.0 + 8.0;
        let color = match crate::roles::role_color(role) {
            Some(c) => c,
            None if speaking => theme::GIZ,
            None => theme::NEVOA,
        };
        let name = crate::app::notify::fit_name(&painter, m.user.shown_name(), theme::body(13.5), color, x - name_left - 2.0);
        painter.galley(Pos2::new(name_left, cy - name.size().y / 2.0), name, color);

        // The parts with their own hover / click, on top of the row's.
        if let Some(r) = local_rect.as_ref() {
            ui.interact(r.0, ui.id().with(("local_audio", m.user.id)), Sense::hover()).on_hover_text(&r.1);
        }
        if let Some(r) = role_rect {
            ui.interact(r, ui.id().with(("role", m.user.id)), Sense::hover()).on_hover_text(crate::roles::role_label(role));
        }
        if let Some(r) = viewers_rect {
            ui.interact(r, ui.id().with(("viewers", m.user.id)), Sense::hover()).on_hover_ui(|ui| {
                let names = self.viewers_of(m.user.id);
                ui.label(if names.is_empty() { App::viewers_text(&names) } else { names.join("\n") });
            });
        }
        if let Some(r) = live_rect {
            let live = ui.interact(r.expand(2.0), ui.id().with(("live", m.user.id)), Sense::click());
            let hover = match (me, watching, audio_only, same_channel) {
                (true, ..) => "Você está ao vivo",
                (false, true, true, _) => "Parar de ouvir",
                (false, true, false, _) => "Parar de assistir",
                (false, false, true, true) => "Ouvir",
                (false, false, false, true) => "Assistir",
                (false, false, true, false) => "Entre neste canal de voz para ouvir",
                (false, false, false, false) => "Entre neste canal de voz para assistir",
            };
            // v1.1: with a hub that keeps previews, a resting pointer gets the hover card (the
            // still picture and what is streamed) instead of the plain tooltip.
            let carded = !audio_only && self.stream_hover(ui, "sidebar", r, live.hovered(), &m.user, Some(hover));
            let live = if carded { live } else { live.on_hover_text(hover) };
            if !me && live.clicked() {
                if watching {
                    self.unwatch(&key);
                } else if same_channel {
                    self.watch_focused(&key);
                }
            }
        }
        let resp = self.profile_hit(ui, resp, &m.user);
        self.menu_hit(&resp, &m.user, Origin::Sidebar, Lead::None);
    }

    // ------------------------------------------------------------ the voice panel

    fn voice_panel(&mut self, ui: &mut egui::Ui) {
        let room = |app: &App, id: Option<vox_proto::ChannelId>| -> String {
            if id == Some(super::dms::DM_CALL_CHANNEL) {
                return app.dm_call_title();
            }
            id.and_then(|i| app.channels.iter().find(|c| c.id == i)).map(|c| c.name.clone()).unwrap_or_default()
        };
        if let Some(err) = self.voice.error.clone() {
            ui.label(RichText::new(err).font(theme::body(12.5)).color(theme::AO_VIVO));
            ui.horizontal(|ui| {
                if let Some(ch) = self.voice.retry {
                    if ui_kit::button_sized(ui, Kind::Secondary, Size::Small, None, "Tentar de novo").clicked() {
                        self.join_voice(ch);
                    }
                }
                if ui_kit::button_sized(ui, Kind::Ghost, Size::Small, None, "Fechar").clicked() {
                    self.voice.error = None;
                    self.voice.retry = None;
                }
            });
            return;
        }
        if let Some(j) = self.voice.joining {
            let secs = self.voice.joining_since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.label(RichText::new("Conectando à voz...").font(theme::semibold(13.0)).color(theme::AVISO));
                    ui.label(RichText::new(format!("{} · {secs} s", room(self, Some(j)))).font(theme::body(12.0)).color(theme::NEVOA));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui_kit::icon_button(ui, ph::X, "Cancelar", IconState::Normal).clicked() {
                        self.leave_voice();
                    }
                });
            });
            return;
        }
        let (state, color) = match self.voice.link {
            super::Link::Up => ("Voz conectada", theme::ONLINE),
            super::Link::Reconnecting => ("Voz: reconectando...", theme::AVISO),
        };
        let name = room(self, self.voice.channel);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.label(RichText::new(state).font(theme::semibold(13.0)).color(color));
                let r = ui.add(egui::Label::new(RichText::new(&name).font(theme::body(12.0)).color(theme::NEVOA)).truncate().sense(Sense::click()));
                if r.on_hover_text("Abrir a sala").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.show_call();
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (hover, dm) = if self.in_dm_call() { ("Desligar", true) } else { ("Sair da voz", false) };
                if ui_kit::icon_button(ui, ph::PHONE_X, hover, IconState::Off).clicked() {
                    if dm {
                        self.hang_up_dm_call();
                    } else {
                        self.leave_voice();
                    }
                }
            });
        });
        if let Some(w) = self.voice.warning.clone() {
            ui.label(RichText::new(format!("{}  {w}", super::ICON_WARN)).font(theme::body(12.0)).color(theme::AVISO));
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let (icon, hover, state) = match &self.voice.camera {
                CameraUi::Off => (ph::VIDEO_CAMERA_SLASH, "Ligar a câmera".to_string(), IconState::Normal),
                CameraUi::Starting => (ph::VIDEO_CAMERA, "Ligando a câmera...".to_string(), IconState::Normal),
                CameraUi::On { info, .. } => (ph::VIDEO_CAMERA, format!("Câmera ligada ({info}). Clique para desligar"), IconState::On),
            };
            let cam_soon = self.voice.camera == CameraUi::Off && crate::platform::camera_soon();
            let hover = if cam_soon { format!("Câmera: {}", crate::platform::SOON_MAC_LC) } else { hover };
            let cam = ui.add_enabled_ui(self.voice.camera != CameraUi::Starting && !cam_soon, |ui| ui_kit::icon_button(ui, icon, &hover, state)).inner;
            if cam.clicked() {
                self.toggle_camera();
            }
            match self.voice.share {
                Share::Off if crate::platform::screen_soon() => {
                    ui.add_enabled_ui(false, |ui| ui_kit::icon_button(ui, ph::SCREENCAST, &format!("Transmitir: {}", crate::platform::SOON_MAC_LC), IconState::Normal));
                }
                Share::Off => {
                    if ui_kit::icon_button(ui, ph::SCREENCAST, "Transmitir (Go Live)", IconState::Normal).clicked() {
                        self.open_golive();
                    }
                }
                Share::Starting => {
                    ui.add_enabled_ui(false, |ui| ui_kit::icon_button(ui, ph::SCREENCAST, "Iniciando transmissão...", IconState::On));
                }
                Share::Live { .. } => {
                    if ui_kit::icon_button(ui, ph::SCREENCAST, "Ao vivo: clique para parar a transmissão", IconState::On).clicked() {
                        self.stop_share();
                    }
                    ui_kit::live_tag(ui);
                }
            }
        });
    }

    // ------------------------------------------------------------ the "você" panel

    fn me_panel(&mut self, ui: &mut egui::Ui) {
        let Some(me) = self.me.clone() else { return };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let pic = crate::avatars::avatar_widget(&mut self.avatars.store, ui, &me, 32.0, crate::avatars::Surface::Sidebar);
            if self.profile.supported == Some(true) {
                let presence = self.my_presence();
                ui.painter().circle(pic.rect.right_bottom() - Vec2::splat(3.0), 5.0, crate::profile::presence_color(presence), egui::Stroke::new(2.0, PANEL_BG));
            }
            let pic = self.profile_hit(ui, pic, &me);
            self.menu_hit(&pic, &me, Origin::Sidebar, Lead::None);
            // The icons take their room first (right to left), the name and status what is left.
            let icons_w = 3.0 * 30.0 + 2.0 * 2.0;
            let text_w = (ui.available_width() - icons_w - 8.0).max(30.0);
            ui.allocate_ui_with_layout(Vec2::new(text_w, 34.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(text_w);
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let name = ui.add(egui::Label::new(RichText::new(me.shown_name()).font(theme::semibold(13.5)).color(theme::GIZ)).truncate().sense(Sense::click()));
                    let name = self.profile_hit(ui, name, &me);
                    self.menu_hit(&name, &me, Origin::Sidebar, Lead::None);
                    if self.roles.supported {
                        crate::roles::role_badge(ui, me.role);
                    }
                });
                self.me_status(ui);
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if ui_kit::icon_button(ui, ph::GEAR_SIX, "Configurações", IconState::Normal).clicked() {
                    self.open_settings();
                }
                self.me_voice_buttons(ui);
            });
        });
    }

    /// The status line under my name: the voice room I am in (green), else my presence (a click
    /// changes it).
    fn me_status(&mut self, ui: &mut egui::Ui) {
        let font = theme::body(12.0);
        if let Some(ch) = self.voice.channel {
            let text = if ch == super::dms::DM_CALL_CHANNEL {
                "em uma chamada".to_string()
            } else {
                format!("na {}", self.channels.iter().find(|c| c.id == ch).map(|c| c.name.as_str()).unwrap_or("voz"))
            };
            ui.add(egui::Label::new(RichText::new(text).font(font).color(theme::ONLINE)).truncate().selectable(false));
            return;
        }
        if self.profile.supported != Some(true) {
            ui.label(RichText::new("online").font(font).color(theme::ONLINE));
            return;
        }
        let current = self.my_presence();
        let r = ui.add(egui::Label::new(RichText::new(format!("{} {}", crate::profile::presence_label(current), ph::CARET_DOWN)).font(font).color(presence_text_color(current))).sense(Sense::click()));
        let r = r.on_hover_text(format!("{}. Clique para mudar.", crate::profile::presence_label(current)));
        let mut choose = None;
        egui::Popup::menu(&r).frame(ui_kit::popup_frame(ui.style())).style(ui_kit::menu_style).show(|ui| {
            ui.set_min_width(200.0);
            for p in vox_proto::Presence::ALL {
                // The presence's colour on its dot; the current one checked (as in the person menu).
                let icon = if current == p { ph::CHECK } else { ph::CIRCLE };
                let row = ui_kit::menu_row_ex(ui, icon, Some(crate::profile::presence_color(p)), crate::profile::presence_label(p), false, false);
                if row.on_hover_text(crate::profile::presence_hint(p)).clicked() {
                    choose = Some(p);
                    ui.close();
                }
            }
        });
        if let Some(p) = choose.filter(|p| *p != current) {
            self.set_presence(p);
        }
    }

    /// Microphone and deafen (right to left: deafen first). They act on the call, so without one
    /// they rest.
    fn me_voice_buttons(&mut self, ui: &mut egui::Ui) {
        let in_call = self.voice.channel.is_some();
        let deaf = self.voice.deafened;
        let r = ui
            .add_enabled_ui(in_call, |ui| ui_kit::icon_button(ui, super::ICON_DEAF, if deaf { "Ouvir de novo" } else { "Ensurdecer" }, if deaf { IconState::Off } else { IconState::Normal }))
            .inner
            .on_disabled_hover_text("Entre em uma sala de voz para usar");
        if r.clicked() {
            self.toggle_deafen();
        }
        let ptt = self.ptt_active();
        let server_muted = self.i_am_server_muted();
        let muted = self.voice.muted && !ptt;
        let (icon, hover) = if server_muted {
            (super::ICON_MUTED, "Silenciado pelo servidor".to_string())
        } else if self.voice.no_mic {
            (super::ICON_MUTED, "Sem microfone".to_string())
        } else if ptt {
            (ph::MICROPHONE, format!("Push-to-talk ({})", self.settings.ptt_name))
        } else if muted {
            (super::ICON_MUTED, "Microfone mutado: clique para falar".to_string())
        } else {
            (ph::MICROPHONE, "Mutar".to_string())
        };
        let state = if server_muted || muted || self.voice.no_mic { IconState::Off } else { IconState::Normal };
        let enabled = in_call && !server_muted && !ptt && !deaf && !self.voice.no_mic;
        let disabled_hover = if !in_call {
            "Entre em uma sala de voz para usar".to_string()
        } else if server_muted {
            "Um moderador silenciou você no servidor: só um moderador pode liberar".to_string()
        } else {
            hover.clone()
        };
        let r = ui.add_enabled_ui(enabled, |ui| ui_kit::icon_button(ui, icon, &hover, state)).inner.on_disabled_hover_text(disabled_hover);
        if r.clicked() {
            self.toggle_mute();
        }
    }

    // ------------------------------------------------------------ the member list (right of the chat)

    pub(super) fn online_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui_kit::section(ui, &format!("Online — {}", self.online.len()));
        });
        ui.add_space(4.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let mut users = self.online.clone();
            users.sort_by_key(|u| u.shown_name().to_lowercase());
            for u in users {
                self.online_row(ui, &u);
            }
        });
    }

    fn online_row(&mut self, ui: &mut egui::Ui, u: &User) {
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ONLINE_H), Sense::click());
        if !ui.is_rect_visible(rect) {
            return;
        }
        let in_voice = self.voice_members.contains_key(&u.id);
        let with_me = self.voice.channel.is_some() && self.my_id() != Some(u.id) && self.voice_members.get(&u.id).is_some_and(|m| m.channel_id == self.voice.channel);
        let ctx = ui.ctx().clone();
        let painter = ui.painter_at(rect);
        let bg = if resp.hovered() { theme::ROW_HOVER } else { theme::PAREDE };
        if resp.hovered() {
            painter.rect_filled(rect, radius::ROW, bg);
        }
        let cy = rect.center().y;
        let center = Pos2::new(rect.left() + 8.0 + ONLINE_AVATAR / 2.0, cy);
        crate::avatars::draw_avatar(&mut self.avatars.store, &painter, &ctx, center, ONLINE_AVATAR / 2.0, u, crate::avatars::Surface::Online);
        // The presence dot on the picture's corner, as on Discord.
        let presence = self.presence_of(u.id);
        painter.circle(center + Vec2::splat(ONLINE_AVATAR / 2.0 * 0.72), 5.0, crate::profile::presence_color(presence), egui::Stroke::new(2.0, bg));
        let mut x = rect.right() - 8.0;
        let mut voice_rect = None;
        // v1.1: someone live shows it here too (the hover card works from this list as well).
        let live = self.voice_members.get(&u.id).is_some_and(|m| m.streaming && !m.audio_only && m.channel_id.is_some());
        if in_voice {
            voice_rect = Some(if live { icon_at(&painter, &mut x, cy, ph::SCREENCAST, theme::AO_VIVO) } else { icon_at(&painter, &mut x, cy, super::ICON_VOICE, theme::NEVOA) });
        }
        let mut local = None;
        if with_me {
            let pref = self.settings.user_audio(&u.username);
            if pref.muted {
                local = Some((icon_at(&painter, &mut x, cy, super::ICON_SPEAKER_OFF, theme::GIZ), "Silenciado só para você".to_string()));
            } else if pref.volume_pct != 100 {
                local = Some((icon_at(&painter, &mut x, cy, super::ICON_VOL, theme::GIZ), format!("Volume ajustado só para você: {}%", pref.volume_pct)));
            }
        }
        let status = self.status_of(u.id);
        let mut status_rect = None;
        if let Some(st) = &status {
            let e = if st.emoji.is_empty() { ph::CHAT_CENTERED_TEXT } else { st.emoji.as_str() };
            status_rect = Some(icon_at(&painter, &mut x, cy, e, theme::GIZ));
        }
        let role = if self.roles.supported { u.role } else { Default::default() };
        let role_rect = match (crate::roles::role_icon(role), crate::roles::role_color(role)) {
            (Some(icon), Some(color)) => Some(icon_at(&painter, &mut x, cy, icon, color)),
            _ => None,
        };
        let name_left = center.x + ONLINE_AVATAR / 2.0 + 10.0;
        let color = crate::roles::role_color(role).unwrap_or(if resp.hovered() { theme::GIZ } else { theme::TEXTO });
        let name = crate::app::notify::fit_name(&painter, u.shown_name(), theme::body(14.0), color, x - name_left - 2.0);
        painter.galley(Pos2::new(name_left, cy - name.size().y / 2.0), name, color);
        if let Some(r) = voice_rect {
            let hit = ui.interact(r.expand(3.0), ui.id().with(("in_voice", u.id)), Sense::hover());
            let carded = live && self.stream_hover(ui, "online", r, hit.hovered(), u, Some("Ao vivo em um canal de voz"));
            if !carded {
                hit.on_hover_text(if live { "Ao vivo em um canal de voz" } else { "Em um canal de voz" });
            }
        }
        if let Some((r, t)) = local {
            ui.interact(r, ui.id().with(("local", u.id)), Sense::hover()).on_hover_text(t);
        }
        if let (Some(r), Some(st)) = (status_rect, status) {
            ui.interact(r, ui.id().with(("status", u.id)), Sense::hover()).on_hover_text(crate::profile::status_line(&st));
        }
        if let Some(r) = role_rect {
            ui.interact(r, ui.id().with(("role", u.id)), Sense::hover()).on_hover_text(crate::roles::role_label(role));
        }
        let resp = self.profile_hit(ui, resp, u);
        self.menu_hit(&resp, u, Origin::Online, Lead::None);
    }
}

/// The colour of the presence line under my name: the dot's colour, but readable (the switched-off
/// grey of "Invisível" is for dots, too dark for text).
fn presence_text_color(p: vox_proto::Presence) -> Color32 {
    let c = crate::profile::presence_color(p);
    if c == crate::profile::OFFLINE_COLOR {
        theme::NEVOA
    } else {
        c
    }
}
