//! The voice dock (v1.0, a child module of `call`): the call's controls at the bottom of the
//! stage, as in the mockup.
//!
//! - In the middle, big buttons: microphone, camera, "Transmitir" (the lamp: the room's main
//!   action; "Parar" and "Trocar tela" while live), sounds (the soundboard) and leave (red).
//! - On the left, the connection ("Voz conectada · Sala", or reconnecting), a warning when the
//!   call has limitations, and the microphone level under it.
//! - On the right, smaller: deafen, silence everyone, the voice changer and a "⋯" menu (camera
//!   settings, Settings, and while live "Trocar tela" and what the live is).
//!
//! While joining it shows the progress and "Cancelar"; after a failed join, the error with
//! "Tentar de novo" and "Fechar". Everything of the old voice bar (v0.9) is here.

use eframe::egui::{self, Align, Color32, Layout, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Vec2, WidgetInfo, WidgetType};
use vox_client_core::VoiceEffect;

use crate::app::{encoder_label, App, CameraUi, Link, Share};
use crate::theme::{self, ph, radius, space, stroke};
use crate::ui_kit::{self, IconState, Kind};

/// Height of the dock.
pub const DOCK_H: f32 = 64.0;
/// Height of its big buttons.
const BIG: f32 = 40.0;
/// The id of the "⋯" menu's popup (the autotest opens it for a screenshot).
pub const MORE_MENU: &str = "voice_dock_more";
/// The id of the voice changer's popup (the autotest opens it for a screenshot).
pub const FX_MENU: &str = "voice_dock_fx";
/// An icon button of the right group, and the whole group (deafen, silence all, voice changer, ⋯).
const ICON_W: f32 = 30.0;
const SECONDARY_W: f32 = 4.0 * ICON_W + 3.0 * space::S;

/// How much the dock folds on a narrow stage (v1.0 phase 4: the chat docked beside the stage in a
/// 900-point window left the leave button cut off). 0: nothing; 1: "Transmitir" (or "Parar")
/// without its label; 2: the sounds button into the "⋯" menu as well. `width`: the dock's inner
/// width.
fn fold_level(width: f32) -> u8 {
    if width >= 430.0 {
        0
    } else if width >= 340.0 {
        1
    } else {
        2
    }
}

/// How a big dock button looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Look {
    /// Quiet ([`theme::MOVEL`]).
    Normal,
    /// Switched on (the camera): a lighter fill.
    On,
    /// Something normally on is off (a muted microphone): red icon on a dark red fill.
    Off,
    /// The main action (the lamp).
    Primary,
    /// Leaving.
    Danger,
}

/// A big button of the dock: a Phosphor icon and an optional label. Disabled buttons still show
/// their hover text.
fn dock_button(ui: &mut egui::Ui, icon: &str, text: &str, look: Look, enabled: bool, hover: &str) -> egui::Response {
    let painter = ui.painter();
    let label = (!text.is_empty()).then(|| painter.layout_no_wrap(text.to_owned(), theme::semibold(theme::size::BUTTON), Color32::PLACEHOLDER));
    let glyph = painter.layout_no_wrap(icon.to_owned(), theme::body(19.0), Color32::PLACEHOLDER);
    let gap = if label.is_some() { space::M } else { 0.0 };
    let content = glyph.size().x + gap + label.as_ref().map_or(0.0, |g| g.size().x);
    let pad = if label.is_some() { space::L + 2.0 } else { 0.0 };
    let w = (content + 2.0 * pad).max(BIG + 4.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, BIG), if enabled { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        let (hovered, pressed) = (enabled && resp.hovered(), enabled && resp.is_pointer_button_down_on());
        let (mut fill, mut fg) = match look {
            Look::Primary => ui_kit::button_colors(Kind::Primary, hovered, pressed),
            Look::Danger => ui_kit::button_colors(Kind::Danger, hovered, pressed),
            Look::Normal => ui_kit::button_colors(Kind::Secondary, hovered, pressed),
            Look::On => (if hovered { theme::MOVEL_HOVER } else { theme::RISCO }, theme::GIZ),
            Look::Off => (if hovered { theme::PERIGO_FUNDO.gamma_multiply(1.3) } else { theme::PERIGO_FUNDO }, theme::AO_VIVO),
        };
        if !enabled {
            fill = theme::MOVEL_PRESSED;
            fg = fg.gamma_multiply(0.6);
        }
        let painter = ui.painter();
        painter.rect_filled(rect, radius::BUTTON, fill);
        if resp.has_focus() {
            painter.rect_stroke(rect, radius::BUTTON, Stroke::new(stroke::FOCUS, theme::GIZ), StrokeKind::Inside);
        }
        let mut x = rect.center().x - content / 2.0;
        let gw = glyph.size().x;
        painter.galley(Pos2::new(x, rect.center().y - glyph.size().y / 2.0), glyph, fg);
        x += gw + gap;
        if let Some(g) = label {
            painter.galley(Pos2::new(x, rect.center().y - g.size().y / 2.0), g, fg);
        }
    }
    let name = if text.is_empty() { hover } else { text };
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, name));
    if hover.is_empty() {
        resp
    } else {
        resp.on_hover_text(hover)
    }
}

impl App {
    /// The voice dock in `rect` (the bottom of the stage).
    pub(crate) fn voice_dock(&mut self, ui: &mut egui::Ui, rect: Rect) {
        ui.painter().hline(rect.x_range(), rect.top() + 0.5, Stroke::new(stroke::HAIRLINE, theme::RISCO));
        let inner = rect.shrink2(Vec2::new(14.0, 0.0));
        if let Some(err) = self.voice.error.clone() {
            self.dock_error(ui, inner, &err);
            return;
        }
        if let Some(j) = self.voice.joining {
            self.dock_joining(ui, inner, j);
            return;
        }
        if self.voice.channel.is_none() {
            return;
        }
        // The middle group, centred with its width from the last frame. On a narrow stage (the
        // chat docked beside it) the secondary buttons fold into the "⋯" menu, and the middle
        // group moves left if even that does not fit.
        let cw = if self.call.dock_center_w > 0.0 { self.call.dock_center_w } else { 360.0 };
        let mut left_x = (inner.center().x - cw / 2.0).max(inner.left());
        let right_space = inner.right() - (left_x + cw) - space::L;
        let compact = right_space < SECONDARY_W;
        if right_space < ICON_W {
            left_x = (inner.right() - ICON_W - space::L - cw).max(inner.left());
        }
        let mid = Rect::from_min_max(Pos2::new(left_x, inner.top()), inner.max);
        let mut c = ui.new_child(egui::UiBuilder::new().max_rect(mid).layout(Layout::left_to_right(Align::Center)));
        c.spacing_mut().item_spacing.x = space::M + 2.0;
        let fold = fold_level(inner.width());
        self.dock_main_buttons(&mut c, fold);
        let used = c.min_rect().width();
        if (used - cw).abs() > 0.5 {
            self.call.dock_center_w = used;
            ui.ctx().request_repaint();
        }
        let mid_right = left_x + used;
        // The secondary group on the right.
        let right = Rect::from_min_max(Pos2::new(mid_right + space::L, inner.top()), inner.max);
        let mut r = ui.new_child(egui::UiBuilder::new().max_rect(right).layout(Layout::right_to_left(Align::Center)));
        r.spacing_mut().item_spacing.x = space::S;
        self.dock_secondary(&mut r, compact || fold > 0, fold >= 2);
        // The status on the left, in what is left of the space.
        let left = Rect::from_min_max(inner.min, Pos2::new(left_x - space::L, inner.bottom()));
        if left.width() > 24.0 {
            self.dock_status(ui, left);
        }
    }

    /// Microphone, camera, Transmitir, sounds, leave (`fold`: see [`fold_level`]).
    fn dock_main_buttons(&mut self, ui: &mut egui::Ui, fold: u8) {
        // Microphone.
        let ptt = self.ptt_active();
        let server_muted = self.i_am_server_muted();
        let muted = self.voice.muted && !ptt;
        let (icon, look, hover) = if server_muted {
            (ph::MICROPHONE_SLASH, Look::Off, "Um moderador silenciou você no servidor: só um moderador pode liberar".to_string())
        } else if self.voice.no_mic {
            (ph::MICROPHONE_SLASH, Look::Normal, "Sem microfone: você está só ouvindo".to_string())
        } else if ptt {
            (ph::MICROPHONE, Look::Normal, format!("Push-to-talk ({}): a tecla decide quando você fala", self.settings.ptt_name))
        } else if self.voice.deafened {
            (ph::MICROPHONE_SLASH, Look::Off, "Ensurdecido: o microfone volta quando você ouvir de novo".to_string())
        } else if muted {
            (ph::MICROPHONE_SLASH, Look::Off, "Microfone mutado: clique para desmutar".to_string())
        } else {
            (ph::MICROPHONE, Look::Normal, "Mutar o microfone".to_string())
        };
        let enabled = !server_muted && !ptt && !self.voice.deafened && !self.voice.no_mic;
        let text = if ptt { "PTT" } else { "" };
        if dock_button(ui, icon, text, look, enabled, &hover).clicked() {
            self.toggle_mute();
        }
        // Camera.
        let (icon, text, look, hover, enabled) = match &self.voice.camera {
            CameraUi::Off if crate::platform::camera_soon() => (ph::VIDEO_CAMERA_SLASH, "", Look::Normal, format!("Câmera: {}", crate::platform::SOON_MAC_LC), false),
            CameraUi::Off => (ph::VIDEO_CAMERA_SLASH, "", Look::Normal, "Ligar a câmera (escolha a câmera e a qualidade em Configurações)".to_string(), true),
            CameraUi::Starting => (ph::VIDEO_CAMERA, "Ligando...", Look::Normal, String::new(), false),
            CameraUi::On { encoder, info } => (ph::VIDEO_CAMERA, "", Look::On, format!("Câmera ligada: {info} · {}\nClique para desligar", encoder_label(encoder)), true),
        };
        if dock_button(ui, icon, text, look, enabled, &hover).clicked() {
            self.toggle_camera();
        }
        // Go Live.
        match self.voice.share.clone() {
            Share::Off => {
                let text = if fold >= 1 { "" } else { "Transmitir" };
                let soon = crate::platform::screen_soon();
                let hover = if soon { format!("Transmitir: {}", crate::platform::SOON_MAC_LC) } else { "Transmitir uma janela, a tela inteira ou só o áudio de um app".to_string() };
                if dock_button(ui, ph::SCREENCAST, text, Look::Primary, !soon, &hover).clicked() {
                    self.open_golive();
                }
            }
            Share::Starting => {
                let text = if fold >= 1 { "" } else { "Iniciando..." };
                dock_button(ui, ph::SCREENCAST, text, Look::Primary, false, "Iniciando a transmissão...");
            }
            Share::Live { encoder } => {
                let hover = format!("{}\nClique para encerrar a transmissão", self.live_info(&encoder));
                if dock_button(ui, ph::STOP, if fold >= 1 { "" } else { "Parar" }, Look::On, true, &hover).clicked() {
                    self.stop_share();
                }
                let audio_only = self.voice.share_audio_only;
                let switching = self.voice.switching_to.is_some();
                let hover = if switching {
                    "Trocando..."
                } else if audio_only {
                    "Trocar app: muda o app (ou o som do PC) transmitido sem encerrar a transmissão. Quem ouve continua ouvindo."
                } else {
                    "Trocar tela: muda a janela ou tela transmitida sem encerrar a transmissão. Quem assiste continua assistindo."
                };
                if dock_button(ui, ph::ARROWS_CLOCKWISE, "", Look::Normal, !switching, hover).clicked() {
                    self.open_golive_switch();
                }
            }
        }
        // The soundboard (voice channels only, and only on a hub that has one).
        if fold < 2 && self.sb.supported && !self.in_dm_call() {
            let look = if self.sb.open { Look::On } else { Look::Normal };
            if dock_button(ui, ph::MUSIC_NOTES, "", look, true, "Sons: o soundboard do servidor (todos na sala ouvem)").clicked() {
                self.sb.open = !self.sb.open;
            }
        }
        // Leave.
        if self.in_dm_call() {
            if dock_button(ui, ph::PHONE_X, "", Look::Danger, true, "Desligar a chamada").clicked() {
                self.hang_up_dm_call();
            }
        } else if dock_button(ui, ph::PHONE_X, "", Look::Danger, true, "Sair da voz").clicked() {
            self.leave_voice();
        }
    }

    /// "Ao vivo · 1080p30 · GPU (NVENC)" / "Ao vivo · só áudio · 128 kbps", and who is watching.
    fn live_info(&self, encoder: &str) -> String {
        let mut s = if self.voice.share_audio_only {
            format!("Ao vivo · só áudio · {}", if self.voice.share_audio_high { "256 kbps" } else { "128 kbps" })
        } else {
            format!("Ao vivo · {} · {}", self.voice.share_preset.label(), encoder_label(encoder))
        };
        if self.viewers_shown() {
            if let Some(me) = self.my_id() {
                s.push('\n');
                s.push_str(&Self::viewers_text(&self.viewers_of(me)));
            }
        }
        s
    }

    /// Deafen, silence everyone, the voice changer and the "⋯" menu (right to left). `compact`:
    /// only the "⋯" menu, with the other three inside it; `sounds`: the soundboard too (the
    /// narrowest dock, [`fold_level`] 2).
    fn dock_secondary(&mut self, ui: &mut egui::Ui, compact: bool, sounds: bool) {
        let fx_ok = !self.voice.no_mic && !self.in_dm_call();
        let more = ui_kit::icon_button(ui, ph::DOTS_THREE, "Mais opções", IconState::Normal);
        let frame = ui_kit::popup_frame(ui.style());
        egui::Popup::menu(&more).id(egui::Id::new(MORE_MENU)).frame(frame).style(ui_kit::menu_style).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
            // (At most this wide: the status lines at the end wrap instead of stretching the menu
            // across the stage; v1.0 phase 4.)
            ui.set_min_width(240.0);
            ui.set_max_width(280.0);
            if sounds && self.sb.supported && !self.in_dm_call() {
                let text = if self.sb.open { "Fechar os sons" } else { "Sons" };
                if ui_kit::menu_row(ui, ph::MUSIC_NOTES, text, false).on_hover_text("O soundboard do servidor (todos na sala ouvem)").clicked() {
                    self.sb.open = !self.sb.open;
                    ui.close();
                }
            }
            if compact {
                let deaf = if self.voice.deafened { "Ouvir de novo" } else { "Ensurdecer" };
                if ui_kit::menu_row(ui, ph::HEADPHONES, deaf, false).clicked() {
                    self.toggle_deafen();
                    ui.close();
                }
                let (icon, text) = if self.voice.others_muted { (ph::SPEAKER_HIGH, "Ouvir todos") } else { (ph::SPEAKER_SLASH, "Silenciar todos") };
                if ui_kit::menu_row(ui, icon, text, false).on_hover_text("Só você deixa de ouvir os outros; seu microfone continua como está").clicked() {
                    self.toggle_others_muted();
                    ui.close();
                }
                if fx_ok {
                    ui_kit::menu_separator(ui);
                    ui_kit::section(ui, "Modificador de voz");
                    self.voice_fx_controls(ui, true);
                }
                ui_kit::menu_separator(ui);
            }
            if ui_kit::menu_row(ui, ph::GEAR_SIX, "Configurar câmera", false).on_hover_text("Brilho, exposição, foco, formato, girar...").clicked() {
                self.open_camera_window();
                ui.close();
            }
            if ui_kit::menu_row(ui, ph::WRENCH, "Configurações de voz e vídeo", false).clicked() {
                self.open_settings_at(crate::app::SettingsPage::Voice);
                ui.close();
            }
            if let Share::Live { encoder } = self.voice.share.clone() {
                ui_kit::menu_separator(ui);
                let text = if self.voice.share_audio_only { "Trocar app" } else { "Trocar tela" };
                let switching = self.voice.switching_to.is_some();
                if ui.add_enabled_ui(!switching, |ui| ui_kit::menu_row(ui, ph::ARROWS_CLOCKWISE, text, false)).inner.clicked() {
                    self.open_golive_switch();
                    ui.close();
                }
                ui.label(RichText::new(self.live_info(&encoder)).small().color(theme::NEVOA));
            }
            if !self.voice.watching.is_empty() {
                ui_kit::menu_separator(ui);
                let names: Vec<String> = self.voice.watching.iter().map(|w| w.parse::<vox_proto::UserId>().map(|id| self.name_of(id)).unwrap_or_else(|_| w.clone())).collect();
                ui.label(RichText::new(format!("{} Assistindo {}", ph::MONITOR, names.join(", "))).small().color(theme::NEVOA));
            }
        });
        if compact {
            return;
        }
        // The voice changer (the microphone only; not in a private call).
        if fx_ok {
            let e = self.settings.voice_fx.effect;
            let hover = if e == VoiceEffect::Off {
                "Modificador de voz: desligado".to_string()
            } else {
                format!("Modificador de voz: {} ({}%)", e.label(), self.settings.voice_fx.intensity_pct)
            };
            let fx = ui_kit::icon_button(ui, ph::MASK_HAPPY, &hover, if e == VoiceEffect::Off { IconState::Normal } else { IconState::On });
            egui::Popup::menu(&fx).id(egui::Id::new(FX_MENU)).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                self.voice_fx_popup(ui);
            });
        }
        // Silence everyone (only for me).
        let (icon, hover, state) = if self.voice.others_muted {
            (ph::SPEAKER_SLASH, "Ouvir todos de novo", IconState::Off)
        } else {
            (ph::SPEAKER_HIGH, "Silenciar todos: só você deixa de ouvir os outros; seu microfone continua como está", IconState::Normal)
        };
        if ui_kit::icon_button(ui, icon, hover, state).clicked() {
            self.toggle_others_muted();
        }
        // Deafen.
        let (hover, state) = if self.voice.deafened { ("Ouvir de novo", IconState::Off) } else { ("Ensurdecer: não ouvir nada (e mutar o microfone)", IconState::Normal) };
        if ui_kit::icon_button(ui, ph::HEADPHONES, hover, state).clicked() {
            self.toggle_deafen();
        }
    }

    /// "Voz conectada · Sala", a warning when the call has limitations, and the microphone level.
    fn dock_status(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let (state, color) = match self.voice.link {
            Link::Up => ("Voz conectada", theme::ONLINE),
            Link::Reconnecting => ("Reconectando...", theme::AVISO),
        };
        let room = self.room_name();
        let narrow = rect.width() < 130.0;
        let mut col = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(Layout::top_down(Align::Min)));
        col.spacing_mut().item_spacing.y = 3.0;
        // Vertically centred: two short lines.
        col.add_space(((rect.height() - 34.0) / 2.0).max(0.0));
        let mut hover = format!("{state} · {room}");
        if let Some(w) = &self.voice.warning {
            hover.push_str(&format!("\n{} {w}", ph::WARNING));
        }
        let line = col.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let (dot, _) = ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
            ui.painter().circle_filled(dot.center(), 4.0, color);
            if self.voice.warning.is_some() {
                ui.label(RichText::new(ph::WARNING).color(theme::AVISO));
            }
            if !narrow {
                let text = RichText::new(format!("{state} · {room}")).font(theme::semibold(12.5)).color(color);
                ui.add(egui::Label::new(text).truncate().selectable(false));
            }
        });
        line.response.on_hover_text(hover);
        if self.voice.no_mic {
            if !narrow {
                col.label(RichText::new("Só ouvindo (sem microfone)").size(11.5).color(theme::NEVOA));
            }
        } else {
            let g = self.gate_voice.clone();
            let ptt = self.ptt_active();
            // The meter is on screen: its repaint thread runs while it is (one more frame starts it
            // when it just came back, e.g. the stage opened over the chat).
            let frame = ui.ctx().cumulative_frame_nr();
            if self.call.meter_frame.is_none_or(|f| f + 1 < frame) {
                ui.ctx().request_repaint();
            }
            self.call.meter_frame = Some(frame);
            let w = (rect.width() - 4.0).clamp(30.0, 110.0);
            crate::meter::level_bar(&mut col, Some(&g), Vec2::new(w, 6.0)).on_hover_text(if ptt {
                format!("Nível do microfone: {:.0} dB (push-to-talk: a tecla decide)", g.level_db().max(-99.0))
            } else {
                format!("Nível do microfone: {:.0} dB · limite {:.0} dB", g.level_db().max(-99.0), g.threshold_db())
            });
        }
    }

    /// Joining: the progress and "Cancelar". (The UI's own time limit, should the backend never
    /// answer, is the shell's `voice_join_watchdog`, which runs whatever the center shows.)
    fn dock_joining(&mut self, ui: &mut egui::Ui, rect: Rect, channel: vox_proto::ChannelId) {
        let secs = self.voice.joining_since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let name = if channel == crate::app::dms::DM_CALL_CHANNEL {
            self.dm_call_title()
        } else {
            self.channels.iter().find(|c| c.id == channel).map(|c| c.name.clone()).unwrap_or_default()
        };
        let mut row = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(Layout::left_to_right(Align::Center)));
        row.label(RichText::new(format!("Conectando à voz em {name}... ({secs} s)")).color(theme::AVISO));
        if ui_kit::button(&mut row, Kind::Secondary, None, "Cancelar").clicked() {
            self.leave_voice();
        }
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
    }

    /// A failed join: the message, "Tentar de novo" and "Fechar".
    fn dock_error(&mut self, ui: &mut egui::Ui, rect: Rect, err: &str) {
        let mut row = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(Layout::left_to_right(Align::Center)));
        row.add(egui::Label::new(RichText::new(err).color(theme::AO_VIVO)).truncate());
        if let Some(ch) = self.voice.retry {
            if ui_kit::button(&mut row, Kind::Primary, None, "Tentar de novo").clicked() {
                self.join_voice(ch);
                return;
            }
        }
        if ui_kit::button(&mut row, Kind::Secondary, None, "Fechar").clicked() {
            self.voice.error = None;
            self.voice.retry = None;
        }
    }
}
