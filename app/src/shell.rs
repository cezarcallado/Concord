//! The v1.0 main screen (docs/PLANO-v1.0.md, "Navegação como no Discord"): the sidebar on the
//! left, and the center showing what is open there (`layout.rs`): a text channel's chat (with the
//! member list on the right), a DM, or the call stage, alone or with a text channel docked beside
//! it. The center header carries the mode buttons. A child module of `app`.

use eframe::egui::{self, Align, Layout, Margin, RichText};
use vox_proto::ChannelKind;

use super::{App, Center, JOIN_UI_LIMIT_SECS};
use crate::backend::Cmd;
use crate::layout::{self, CenterMode, Open};
use crate::theme::{self, ph};
use crate::ui_kit::{self, IconState};

/// Height of the center header and of the dock's header.
pub const HEADER_H: f32 = 52.0;
/// The member list's padding (docked, or over the chat).
const MEMBERS_MARGIN: Margin = Margin { left: 8, right: 8, top: 12, bottom: 8 };

/// The frame of a header bar: the night background, side padding.
fn header_frame() -> egui::Frame {
    egui::Frame::new().fill(theme::NOITE).inner_margin(Margin::symmetric(16, 0))
}

/// A header's title: an icon, then the name in the display font (cut with "…" to the room the
/// header's buttons leave: they are laid out first).
fn header_title(ui: &mut egui::Ui, icon: &str, name: &str) -> egui::Response {
    ui.label(RichText::new(icon).font(theme::body(19.0)).color(theme::NEVOA));
    ui.add(egui::Label::new(RichText::new(name).font(theme::display(17.0)).color(theme::GIZ)).truncate().selectable(false))
}

impl App {
    pub(super) fn main_screen(&mut self, ui: &mut egui::Ui) {
        if self.stage_fullscreen && self.call_channel().is_some() {
            // Nothing but the stage (phase 2b toggles the OS fullscreen and handles Esc).
            egui::CentralPanel::no_frame().show(ui, |ui| self.call_stage_ui(ui));
            return;
        }
        if self.call.fullscreen {
            self.fullscreen_view(ui);
            return;
        }
        // The stage needs a call: without one the center goes back to the chat.
        if self.center == Center::Call && self.call_channel().is_none() {
            self.center = Center::Chat;
        }
        self.voice_join_watchdog(ui.ctx());
        // What arrives in the chat on screen (the center's or the docked one) is read while the
        // window is in front (`layout::text_on_screen`; coming back to it clears it, `notify_tick`).
        if let Some(ch) = self.chat_on_screen().filter(|_| self.window_focused()) {
            self.channel_read(ch);
        }
        self.toast_bar(ui);
        self.banners(ui);
        // (I6: at most a share of the window, so a small or zoomed-in window keeps its center.)
        // A width the window forced on it comes back to the one the person chose (session).
        let sidebar = layout::sidebar_range(ui.available_rect_before_wrap().width());
        let (left_id, want_id) = (egui::Id::new("left"), egui::Id::new("sidebar_want"));
        let want = match ui.ctx().data_mut(|d| d.get_persisted::<f32>(want_id)) {
            Some(w) => w,
            None => {
                // (The first frame: the width egui kept from the last session, else the default.)
                let w = egui::PanelState::load(ui.ctx(), left_id).map_or(layout::SIDEBAR_W, |s| s.size().x);
                ui.ctx().data_mut(|d| d.insert_persisted(want_id, w));
                w
            }
        };
        layout::restore_panel_width(ui.ctx(), left_id, want, &sidebar);
        let fit = want.clamp(*sidebar.start(), *sidebar.end());
        let r = egui::Panel::left(left_id)
            .default_size(fit)
            .size_range(sidebar)
            .frame(egui::Frame::new().fill(theme::PAREDE))
            .show(ui, |ui| self.left_panel(ui));
        let w = r.response.rect.width();
        if (w - fit).abs() > 0.5 && ui.input(|i| i.pointer.any_down()) {
            // (Dragged by the person.)
            ui.ctx().data_mut(|d| d.insert_persisted(want_id, w));
        }
        match self.effective_mode() {
            CenterMode::Chat => self.chat_center(ui),
            mode => self.stage_center(ui, mode),
        }
        let ctx = ui.ctx().clone();
        self.reaction_picker(&ctx);
        self.delete_message_window(&ctx);
    }

    /// "Conexão perdida" and the dismissable notice, across the top (the kit's notice colours and
    /// icons).
    fn banners(&mut self, ui: &mut egui::Ui) {
        let bar = |fill| egui::Frame::new().fill(fill).inner_margin(Margin::symmetric(16, 8));
        if !self.hub_up {
            egui::Panel::top("banner").resizable(false).show_separator_line(false).frame(bar(theme::MOVEL)).show(ui, |ui| {
                ui_kit::notice_text(ui, ui_kit::Notice::Warning, "Conexão com o servidor perdida. Tentando reconectar...");
            });
        }
        if let Some(msg) = self.notice.clone() {
            let mut dismiss = false;
            egui::Panel::top("notice").resizable(false).frame(bar(theme::PAREDE)).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        dismiss = ui_kit::button_sized(ui, ui_kit::Kind::Secondary, ui_kit::Size::Small, None, "OK").clicked();
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| ui_kit::notice_text(ui, ui_kit::Notice::Info, &msg));
                    });
                });
            });
            if dismiss {
                self.notice = None;
            }
        }
    }

    /// The last-resort limit of a voice join (the backend gives up on its own long before).
    fn voice_join_watchdog(&mut self, ctx: &egui::Context) {
        let Some(j) = self.voice.joining else { return };
        let secs = self.voice.joining_since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        if secs >= JOIN_UI_LIMIT_SECS {
            tracing::warn!(secs, "voice join: UI gave up waiting for the backend");
            self.backend.send(Cmd::LeaveVoice);
            self.voice.joining = None;
            self.voice.joining_since = None;
            self.voice.retry = Some(j);
            self.voice.error = Some("A conexão de voz demorou demais e foi cancelada. Verifique sua internet e tente de novo.".into());
        } else {
            // (The sidebar's counter.)
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }
    }

    /// The mode buttons (right to left: the rightmost first): only chat, only cards, cards with
    /// the chat docked. The ones with cards need a call.
    /// `dock_fits`: the chat fits beside the cards in this window (else "Cards com o chat ao lado"
    /// shows the cards alone and says so).
    fn mode_buttons(&mut self, ui: &mut egui::Ui, dock_fits: bool) {
        let in_call = self.call_channel().is_some();
        if !in_call && ui.available_width() < 480.0 {
            // I6, a narrow header: out of a call these do nothing (only "Só o chat" works, and it
            // is the one on): the room goes to the title and the buttons that do.
            return;
        }
        let now = self.effective_mode();
        let split = if dock_fits { "Cards com o chat ao lado" } else { "Cards com o chat ao lado (não cabe nesta largura: aumente a janela ou use \"Só o chat\")" };
        let buttons = [
            (CenterMode::CardsWithChat, ph::SQUARE_SPLIT_HORIZONTAL, split),
            (CenterMode::Cards, ph::SQUARES_FOUR, "Só os cards"),
            (CenterMode::Chat, ph::CHAT_CENTERED_TEXT, "Só o chat"),
        ];
        for (mode, icon, hover) in buttons {
            let enabled = in_call || mode == CenterMode::Chat;
            let state = if now == mode { IconState::On } else { IconState::Normal };
            let r = ui.add_enabled_ui(enabled, |ui| ui_kit::icon_button(ui, icon, hover, state)).inner;
            let r = r.on_disabled_hover_text("Entre em uma sala de voz para ver os cards");
            if r.clicked() && now != mode {
                self.set_center_mode(mode);
            }
        }
    }

    // ------------------------------------------------------------ chat in the center

    fn chat_center(&mut self, ui: &mut egui::Ui) {
        let open = self.open_in_center();
        match open {
            Open::Dm(dm) => self.dm_center(ui, dm),
            Open::Text(ch) => {
                let channel = self.channels.iter().find(|c| c.id == ch).cloned();
                // I6: the member list is docked only while the chat keeps its minimum width beside
                // it; otherwise it collapses and its button shows it over the chat (`peek`).
                let ctx = ui.ctx().clone();
                let fits = layout::members_fit(ui.available_rect_before_wrap().width());
                let asked = layout::members_peek(&ctx);
                let mut peek = !fits && asked == Some(ch);
                if asked.is_some() && !peek {
                    // (The window has room again, or another channel opened.)
                    layout::set_members_peek(&ctx, None);
                }
                egui::Panel::top("center_head").resizable(false).exact_size(HEADER_H).frame(header_frame()).show(ui, |ui| {
                    let (icon, name) = match &channel {
                        Some(c) if c.private.is_some() => (super::ICON_PRIVATE, c.name.as_str()),
                        Some(c) => (ph::HASH, c.name.as_str()),
                        None => (ph::HASH, ""),
                    };
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        self.mode_buttons(ui, true);
                        ui.add_space(10.0);
                        if fits {
                            let shown = self.settings.layout.show_members;
                            let hover = if shown { "Esconder a lista de membros" } else { "Mostrar a lista de membros" };
                            if ui_kit::icon_button(ui, ph::USERS, hover, if shown { IconState::On } else { IconState::Normal }).clicked() {
                                self.settings.layout.show_members = !shown;
                                self.save_settings();
                            }
                        } else {
                            let hover = if peek { "Esconder a lista de membros" } else { "Mostrar a lista de membros (sobre o chat: a janela está estreita)" };
                            if ui_kit::icon_button(ui, ph::USERS, hover, if peek { IconState::On } else { IconState::Normal }).clicked() {
                                peek = !peek;
                                layout::set_members_peek(&ctx, peek.then_some(ch));
                            }
                        }
                        crate::layout::probe(ui.ctx(), "center_tools", ui.min_rect());
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 10.0;
                            let t = header_title(ui, icon, name);
                            if channel.as_ref().is_some_and(|c| c.private.is_some()) {
                                t.on_hover_text(super::PRIVATE_HOVER);
                            }
                            crate::layout::probe(ui.ctx(), "center_title", ui.min_rect());
                        });
                    });
                });
                if fits && self.settings.layout.show_members {
                    let r = egui::Panel::right("members").resizable(false)
                        .exact_size(layout::MEMBERS_W)
                        .frame(egui::Frame::new().fill(theme::PAREDE).inner_margin(MEMBERS_MARGIN))
                        .show(ui, |ui| self.online_panel(ui));
                    layout::probe(&ctx, "members", r.response.rect);
                }
                let r = egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::NOITE)).show(ui, |ui| self.channel_chat(ui, ch));
                layout::probe(&ctx, "center_chat", r.response.rect);
                if peek {
                    self.members_over_chat(&ctx, r.response.rect);
                }
            }
            Open::Nothing | Open::Stage(_) => {
                egui::Panel::top("center_head").resizable(false).exact_size(HEADER_H).frame(header_frame()).show(ui, |ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| self.mode_buttons(ui, true));
                });
                egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::NOITE)).show(ui, |ui| {
                    ui.centered_and_justified(|ui| {
                        let text = if self.ready { "Escolha um canal de texto." } else { "Carregando..." };
                        ui.label(RichText::new(text).color(theme::NEVOA));
                    });
                });
            }
        }
    }

    /// The member list over the right of the chat (`chat`), while it is collapsed and its button
    /// asked for it (`layout::members_peek`). Esc or the button again hides it.
    fn members_over_chat(&mut self, ctx: &egui::Context, chat: egui::Rect) {
        let w = layout::MEMBERS_W.min(chat.width() - 48.0).max(160.0);
        let rect = egui::Rect::from_min_max(egui::pos2(chat.right() - w, chat.top()), chat.right_bottom());
        let inner = rect.size() - egui::vec2(MEMBERS_MARGIN.sum().x, MEMBERS_MARGIN.sum().y);
        let shadow = ctx.global_style().visuals.popup_shadow;
        let r = egui::Area::new(egui::Id::new("members_over_chat")).order(egui::Order::Middle).fixed_pos(rect.min).constrain(false).show(ctx, |ui| {
            egui::Frame::new().fill(theme::PAREDE).inner_margin(MEMBERS_MARGIN).shadow(shadow).show(ui, |ui| {
                ui.set_min_size(inner);
                ui.set_max_size(inner);
                self.online_panel(ui);
            });
        });
        layout::probe(ctx, "members_peek", r.response.rect);
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !egui::Popup::is_any_open(ctx) && self.cmenu.open.is_none() {
            layout::set_members_peek(ctx, None);
        }
    }

    /// A DM in the center: its header (the person, the call buttons, the options), the note about
    /// where the messages are kept, the lines and the input.
    fn dm_center(&mut self, ui: &mut egui::Ui, dm: vox_proto::DmId) {
        let Some(conv) = self.dms.conv(dm).cloned() else {
            egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::NOITE)).show(ui, |ui| {
                ui.centered_and_justified(|ui| ui.label(RichText::new("Carregando...").color(theme::NEVOA)));
            });
            return;
        };
        let e2e = self.privacy.view.can_seal_for(conv.peer.id);
        let reverify = self.privacy.view.safety.get(&conv.peer.id).is_some_and(|s| s.reverify);
        egui::Panel::top("center_head").resizable(false).exact_size(HEADER_H).frame(header_frame()).show(ui, |ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                self.mode_buttons(ui, true);
                ui.add_space(10.0);
                self.dm_header_actions(ui, dm, &conv);
                layout::probe(ui.ctx(), "center_tools", ui.min_rect());
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    crate::avatars::avatar_widget(&mut self.avatars.store, ui, &conv.peer, 26.0, crate::avatars::Surface::Chat);
                    ui.add(egui::Label::new(RichText::new(conv.peer.shown_name()).font(theme::display(17.0)).color(theme::GIZ)).truncate().selectable(false));
                    ui_kit::header_extra(ui, conv.peer.has_display_name().then(|| format!("@{}", conv.peer.username)), 14.0);
                    // v1.1 (E2): the next line is sealed (a click shows the safety code).
                    if e2e {
                        ui.add_space(6.0);
                        let tag = ui.add(
                            egui::Label::new(RichText::new(format!("{}  {}", ph::LOCK_SIMPLE, crate::backend::e2ee::dm::HEADER_TEXT)).font(theme::body(12.5)).color(theme::ONLINE))
                                .truncate()
                                .sense(egui::Sense::click()),
                        );
                        if tag.on_hover_text(super::dms::SAFETY_TITLE).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            self.dms.safety_for = Some(conv.peer.id);
                        }
                    }
                    layout::probe(ui.ctx(), "center_title", ui.min_rect());
                });
            });
        });
        let name = conv.peer.shown_name().to_string();
        egui::Panel::bottom(egui::Id::new("dm_input_panel")).resizable(false)
            .frame(egui::Frame::new().fill(theme::NOITE).inner_margin(Margin { left: 12, right: 12, top: 4, bottom: 12 }))
            .show_separator_line(false)
            .show(ui, |ui| self.dm_input_box(ui, dm, &name, conv.blocked));
        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::NOITE)).show(ui, |ui| {
            let ui = &mut ui.new_child(egui::UiBuilder::new().max_rect(ui.max_rect().shrink2(egui::vec2(12.0, 0.0))));
            ui.add_space(8.0);
            if e2e {
                ui.add(egui::Label::new(RichText::new(format!("{}  {}", ph::LOCK_SIMPLE, super::dms::HOST_NOTE_E2E)).font(theme::body(12.0)).color(theme::NEVOA)).wrap())
                    .on_hover_text("Criptografada de ponta a ponta: o servidor guarda as mensagens e as imagens cifradas, sem a chave. Quem hospeda vê só quem fala com quem, quando e o tamanho.");
            } else {
                ui.add(egui::Label::new(RichText::new(format!("{}  {}", ph::INFO, super::dms::HOST_NOTE)).font(theme::body(12.0)).color(theme::NEVOA)).wrap())
                    .on_hover_text("Só vocês dois veem esta conversa no concord (nem admins nem o dono pelo app). Mas ela fica gravada no computador que hospeda o servidor, e quem hospeda poderia ler o arquivo.");
            }
            if reverify {
                // v1.1 (E2): the code had been verified and changed since.
                ui.add_space(6.0);
                let mut open = false;
                ui_kit::notice_frame(ui_kit::Notice::Warning).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            open = ui_kit::button_sized(ui, ui_kit::Kind::Secondary, ui_kit::Size::Small, Some(ph::SHIELD), super::dms::REVERIFY).clicked();
                            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                let text = format!("O código de segurança de {} mudou depois que você verificou. {}.", conv.peer.shown_name(), super::dms::REVERIFY);
                                ui_kit::notice_text(ui, ui_kit::Notice::Warning, &text);
                            });
                        });
                    });
                });
                if open {
                    self.dms.safety_for = Some(conv.peer.id);
                }
            }
            if conv.blocked {
                // The block state: a kit notice with the way out right in it.
                ui.add_space(6.0);
                let mut unblock = false;
                ui_kit::notice_frame(ui_kit::Notice::Warning).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            unblock = ui_kit::button_sized(ui, ui_kit::Kind::Secondary, ui_kit::Size::Small, Some(ph::CHECK), "Desbloquear").clicked();
                            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                let text = format!("Você bloqueou {}. Desbloqueie para conversar e ligar de novo.", conv.peer.shown_name());
                                ui_kit::notice_text(ui, ui_kit::Notice::Warning, &text);
                            });
                        });
                    });
                });
                if unblock {
                    self.block_user(&conv.peer, false);
                }
            }
            self.dm_messages(ui, dm, &conv.peer, conv.blocked);
        });
    }

    // ------------------------------------------------------------ the stage

    fn stage_center(&mut self, ui: &mut egui::Ui, mode: CenterMode) {
        // I6: the docked chat narrows so the stage keeps its minimum, and goes (the cards stay)
        // when even its own minimum does not fit; it comes back when the window has room.
        let dock_range = layout::dock_range(ui.available_rect_before_wrap().width());
        if self.call.dock_squeezed != dock_range.is_none() {
            // (`effective_mode` follows from the next frame on.)
            self.call.dock_squeezed = dock_range.is_none();
            ui.ctx().request_repaint();
        }
        if mode == CenterMode::CardsWithChat {
            if let (Some(ch), Some(range)) = (self.docked_channel(), dock_range.clone()) {
                let width = self.settings.layout.dock_width.clamp(*range.start(), *range.end());
                let max = *range.end();
                layout::restore_panel_width(ui.ctx(), egui::Id::new("dock"), self.settings.layout.dock_width, &range);
                let r = egui::Panel::right("dock")
                    .default_size(width)
                    .size_range(range)
                    .resizable(true)
                    .frame(egui::Frame::new().fill(theme::NOITE))
                    .show(ui, |ui| self.dock(ui, ch));
                layout::probe(ui.ctx(), "dock", r.response.rect);
                // Remember the width once the drag is over (not a width the window forced on it).
                let w = r.response.rect.width();
                let forced = w >= max - 1.0 && self.settings.layout.dock_width > w;
                if (w - self.settings.layout.dock_width).abs() > 1.0 && !forced && !ui.input(|i| i.pointer.any_down()) {
                    self.settings.layout.dock_width = w;
                    self.save_settings();
                }
            }
        }
        let title = match self.call_channel() {
            // (Without its phone icon: the header draws its own.)
            Some(super::dms::DM_CALL_CHANNEL) => self.dm_call_title().trim_start_matches(ph::PHONE).trim().to_string(),
            Some(ch) => self.channels.iter().find(|c| c.id == ch).map(|c| c.name.clone()).unwrap_or_default(),
            None => String::new(),
        };
        let meta = self.stage_header_meta();
        egui::Panel::top("center_head").resizable(false).exact_size(HEADER_H).frame(header_frame()).show(ui, |ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                self.mode_buttons(ui, dock_range.is_some());
                ui.add_space(10.0);
                self.stage_header_tools(ui);
                layout::probe(ui.ctx(), "center_tools", ui.min_rect());
                // (The buttons first, right to left; the title and the counts get what is left.)
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    let icon = if self.in_dm_call() { ph::PHONE } else { ph::SPEAKER_HIGH };
                    header_title(ui, icon, &title);
                    // v1.1 (E1): "Criptografada de ponta a ponta".
                    self.e2ee_header_badge(ui);
                    ui_kit::header_extra(ui, (!meta.is_empty()).then_some(meta), 13.0);
                    layout::probe(ui.ctx(), "center_title", ui.min_rect());
                });
            });
        });
        let r = egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::NOITE)).show(ui, |ui| self.call_stage_ui(ui));
        crate::layout::probe(ui.ctx(), "stage", r.response.rect);
    }

    /// The chat docked beside the cards: its header (the channel, a picker of the text channels,
    /// hide) and the chat itself. It never takes the sidebar's "open" highlight.
    fn dock(&mut self, ui: &mut egui::Ui, ch: vox_proto::ChannelId) {
        let channel = self.channels.iter().find(|c| c.id == ch).cloned();
        egui::Panel::top("dock_head").resizable(false).exact_size(HEADER_H).frame(egui::Frame::new().fill(theme::NOITE).inner_margin(Margin::symmetric(12, 0))).show(ui, |ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui_kit::icon_button(ui, ph::CARET_RIGHT, "Esconder o chat (só os cards)", IconState::Normal).clicked() {
                    self.set_center_mode(CenterMode::Cards);
                }
                crate::layout::probe(ui.ctx(), "dock_tools", ui.min_rect());
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let (icon, name) = match &channel {
                        Some(c) if c.private.is_some() => (super::ICON_PRIVATE, c.name.as_str()),
                        Some(c) => (ph::HASH, c.name.as_str()),
                        None => (ph::HASH, ""),
                    };
                    let picker = self.dock_title_button(ui, icon, name);
                    crate::layout::probe(ui.ctx(), "dock_title", picker.rect);
                    egui::Popup::menu(&picker).frame(ui_kit::popup_frame(ui.style())).show(|ui| {
                        ui.set_min_width(220.0);
                        ui_kit::section(ui, "Chat ao lado dos cards");
                        ui.add_space(2.0);
                        let texts: Vec<(vox_proto::ChannelId, String, bool)> =
                            self.channels.iter().filter(|c| c.kind == ChannelKind::Text).map(|c| (c.id, c.name.clone(), c.private.is_some())).collect();
                        for (id, name, private) in texts {
                            let icon = if private { super::ICON_PRIVATE } else { ph::HASH };
                            if ui_kit::menu_row(ui, icon, &name, false).clicked() {
                                self.dock_channel(id);
                                ui.close();
                            }
                        }
                    });
                });
            });
        });
        self.channel_chat(ui, ch);
    }

    /// The dock's title: the channel and a caret (a click opens the channel picker).
    fn dock_title_button(&mut self, ui: &mut egui::Ui, icon: &str, name: &str) -> egui::Response {
        let p = ui.painter();
        let icon_g = p.layout_no_wrap(icon.to_owned(), theme::body(19.0), theme::NEVOA);
        let name_g = crate::app::notify::fit_name(p, name, theme::display(17.0), theme::GIZ, (ui.available_width() - 70.0).max(40.0));
        let caret = p.layout_no_wrap(ph::CARET_DOWN.to_owned(), theme::body(14.0), theme::NEVOA);
        let w = icon_g.size().x + 8.0 + name_g.size().x + 6.0 + caret.size().x + 12.0;
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 34.0), egui::Sense::click());
        if ui.is_rect_visible(rect) {
            let p = ui.painter();
            if resp.hovered() {
                p.rect_filled(rect, theme::radius::ROW, theme::ROW_HOVER);
            }
            let cy = rect.center().y;
            let mut x = rect.left() + 6.0;
            let (iw, nw) = (icon_g.size().x, name_g.size().x);
            p.galley(egui::pos2(x, cy - icon_g.size().y / 2.0), icon_g, theme::NEVOA);
            x += iw + 8.0;
            p.galley(egui::pos2(x, cy - name_g.size().y / 2.0), name_g, theme::GIZ);
            x += nw + 6.0;
            p.galley(egui::pos2(x, cy - caret.size().y / 2.0), caret, theme::NEVOA);
        }
        resp.on_hover_text("Escolher o canal de texto deste chat")
    }
}
