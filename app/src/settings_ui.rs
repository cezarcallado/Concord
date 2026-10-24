//! Configurações (v1.0, phase 3a): a Discord-like window, the sections in a column on the left
//! and the chosen one on the right (a child module of `app`, so it uses the app's private helpers).
//!
//! Sections ([`SettingsPage`]):
//! - Conta e perfil: profile picture, profile (name, bio, colour, status, presence, idle time),
//!   personal password (and "claim owner").
//! - Voz: devices and output volume, the mic test with the level meter and the input sensitivity,
//!   noise suppression, push-to-talk, the voice changer, alert sounds.
//! - Atalhos (v1.1): keyboard shortcuts, push-to-talk's key among them (`keybinds_ui.rs`).
//! - Vídeo: the camera (summary and "Configurar câmera…"), hardware acceleration and the encoder,
//!   the Go Live defaults (quality, app audio, my own preview), watching streams (separate window,
//!   hear only the pinned ones).
//! - Notificações: Windows notifications, the tray, the alert palette ("Som dos avisos", with a
//!   preview), chat sounds (DMs, other channels, calls).
//! - Chat: link previews, images shown automatically, quality of the images I send.
//! - Sons: the soundboard (mute, volume, sounds silenced one by one).
//! - Sobre: version, the logs folder.
//! - Apoiar o projeto (v1.1): the Pix, only when one is set (`donate_ui.rs`).
//!
//! The window is an egui area (no egui title bar), sized to the app window, with no backdrop: the
//! calls' videos keep playing around it (the airspace code cuts them where it covers them).

use std::path::PathBuf;
use std::sync::OnceLock;

use eframe::egui::{self, Align, Align2, Color32, CornerRadius, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke, UiBuilder, Vec2};
use vox_client_core::audio::gate::{HOLD_MAX_MS, HOLD_MIN_MS, THRESHOLD_MAX_DB, THRESHOLD_MIN_DB};
use vox_client_core::{GateMode, NoiseLevel};

use super::notify::WinNotify;
use super::{App, MicTestUi, FOCUS_AUDIO_HOVER, FOCUS_AUDIO_LABEL, NOISE_FALLBACK_TEXT};
use crate::alerts;
use crate::backend::Cmd;
use crate::forms::{self, note, note_colored, row};
use crate::settings::Preset;
use crate::theme::{self, ph, size, space, stroke};
use crate::ui_kit::{self, IconState, Kind, Size};
use crate::meter;

/// The sections of the settings window, in the order of its left column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsPage {
    #[default]
    Account,
    Voice,
    Keybinds,
    Video,
    Notifications,
    Chat,
    Sounds,
    /// v1.1 (E0): "Seus aparelhos" (`privacy_ui.rs`).
    Privacy,
    About,
    /// v1.1 (D1): "Apoiar o projeto" (`donate_ui.rs`), never in [`Self::ALL`]: see [`Self::shown`].
    Donate,
}

impl SettingsPage {
    pub const ALL: [SettingsPage; 9] = [
        SettingsPage::Account,
        SettingsPage::Voice,
        SettingsPage::Keybinds,
        SettingsPage::Video,
        SettingsPage::Notifications,
        SettingsPage::Chat,
        SettingsPage::Sounds,
        SettingsPage::Privacy,
        SettingsPage::About,
    ];

    /// The rows of the left column: "Apoiar o projeto" only when there is a Pix.
    fn shown() -> impl Iterator<Item = SettingsPage> {
        Self::ALL.into_iter().chain(crate::donate::pix().map(|_| SettingsPage::Donate))
    }

    pub fn title(self) -> &'static str {
        match self {
            SettingsPage::Account => "Conta e perfil",
            SettingsPage::Voice => "Voz",
            SettingsPage::Keybinds => "Atalhos",
            SettingsPage::Video => "Vídeo",
            SettingsPage::Notifications => "Notificações",
            SettingsPage::Chat => "Chat",
            SettingsPage::Sounds => "Sons",
            SettingsPage::Privacy => super::privacy_ui::PAGE_TITLE,
            SettingsPage::About => "Sobre",
            SettingsPage::Donate => super::donate_ui::PAGE_TITLE,
        }
    }

    fn icon(self) -> &'static str {
        match self {
            SettingsPage::Account => ph::USER,
            SettingsPage::Voice => ph::MICROPHONE,
            SettingsPage::Keybinds => ph::KEYBOARD,
            SettingsPage::Video => ph::VIDEO_CAMERA,
            SettingsPage::Notifications => ph::BELL,
            SettingsPage::Chat => ph::CHAT_CENTERED_TEXT,
            SettingsPage::Sounds => ph::MUSIC_NOTES,
            SettingsPage::Privacy => ph::SHIELD,
            SettingsPage::About => ph::INFO,
            SettingsPage::Donate => ph::HEART,
        }
    }

    /// A plain name for files and logs (the screenshots: `settings-<slug>.png`).
    pub fn slug(self) -> &'static str {
        match self {
            SettingsPage::Account => "conta",
            SettingsPage::Voice => "voz",
            SettingsPage::Keybinds => "atalhos",
            SettingsPage::Video => "video",
            SettingsPage::Notifications => "notificacoes",
            SettingsPage::Chat => "chat",
            SettingsPage::Sounds => "sons",
            SettingsPage::Privacy => "privacidade",
            SettingsPage::About => "sobre",
            SettingsPage::Donate => "apoiar",
        }
    }
}

/// Width of the left column.
const NAV_W: f32 = 212.0;
/// The left column's width for a settings window this wide (I6): the names while the page keeps
/// a readable width beside them ([`ui_kit::PAGE_MIN_W`], the server window's rule too), else only
/// the icons.
fn nav_width(window_w: f32) -> f32 {
    if window_w - NAV_W >= ui_kit::PAGE_MIN_W {
        NAV_W
    } else {
        ui_kit::NAV_ICONS_W
    }
}

/// The left column's fill: between [`theme::NOITE`] and [`theme::PAREDE`] (the mockup's user panel).
const NAV_BG: Color32 = Color32::from_rgb(0x17, 0x22, 0x2D);
/// Height of the page header (its title and the ✕).
const HEAD_H: f32 = 58.0;
/// The readable width of a page.
const PAGE_MAX_W: f32 = 680.0;

/// This run's logs folder ("Sobre" → "Abrir pasta dos registros"), set once at startup.
static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn set_log_dir(dir: PathBuf) {
    let _ = LOG_DIR.set(dir);
}

/// "Ouvir" of "Som dos avisos" while it plays (egui temp data, so the preview needs no field of
/// the app): when it started (egui time, seconds) and how many of its sounds already went out.
#[derive(Clone, Copy)]
struct PalettePreview {
    started: f64,
    played: usize,
}

fn palette_preview_id() -> egui::Id {
    egui::Id::new("alert_palette_preview")
}

/// What the page changed this frame (applied after the window is drawn).
#[derive(Default)]
struct Changes {
    /// Something to write to settings.json.
    save: bool,
    /// Push-to-talk changed (re-arms the hook, which also saves).
    ptt: bool,
    gate: bool,
    toggle_test: bool,
    test_alert: bool,
}

impl App {
    /// Open the settings on `page`.
    pub fn open_settings_at(&mut self, page: SettingsPage) {
        self.settings_ui.page = page;
        self.open_settings();
    }

    pub(super) fn settings_window(&mut self, ctx: &egui::Context) {
        // "Configurar câmera" opened from here: Settings waits behind it (shown again on close).
        if !self.settings_ui.open || self.settings_ui.cam.window {
            return;
        }
        let mut ch = Changes::default();
        let mut close = false;
        let devices_before = (self.settings.input_device.clone(), self.settings.output_device.clone());
        let screen = ctx.content_rect();
        // (I6: its minimum only while the window has room for it: never past the window's edges.)
        let size = ui_kit::fit_size(ctx, Vec2::new((screen.width() - 56.0).min(960.0), (screen.height() - 44.0).min(660.0)), Vec2::new(560.0, 360.0), 8.0);
        let nav_w = nav_width(size.x);
        egui::Area::new(egui::Id::new("settings_window")).order(egui::Order::Middle).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            forms::Dialog::frame().show(ui, |ui| {
                ui.set_min_size(size);
                ui.set_max_size(size);
                let full = Rect::from_min_size(ui.max_rect().min, size);
                self.settings_nav(ui, full, nav_w);
                let content = Rect::from_min_max(Pos2::new(full.left() + nav_w, full.top()), full.max);
                close |= self.settings_page(ui, content, &mut ch);
                ui.allocate_rect(full, Sense::hover());
            });
        });
        let escape = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        if escape && !self.settings_ui.capturing_key && !self.keys.capture_just_ended() && !egui::Popup::is_any_open(ctx) {
            close = true;
        }
        self.apply_settings_changes(ch, devices_before, close);
    }

    /// The left column: the window's name and one row per section.
    fn settings_nav(&mut self, ui: &mut egui::Ui, full: Rect, nav_w: f32) {
        let nav = Rect::from_min_size(full.min, Vec2::new(nav_w, full.height())).shrink(1.0);
        let r = theme::radius::POPUP;
        ui.painter().rect_filled(nav, CornerRadius { nw: r - 1, sw: r - 1, ne: 0, se: 0 }, NAV_BG);
        ui.painter().vline(nav.right(), nav.y_range(), Stroke::new(stroke::HAIRLINE, theme::RISCO));
        if nav_w < NAV_W {
            // I6, a narrow window: the sections as icons (their names on hover), in a scroll.
            let mut n = ui.new_child(UiBuilder::new().max_rect(nav.shrink2(Vec2::new(8.0, 12.0))).layout(Layout::top_down(Align::Center)));
            // (The open section's icon is scrolled into view when it changes: opened from elsewhere.)
            let shown_id = egui::Id::new("settings_nav_icons_shown");
            let reveal = ui.ctx().data(|d| d.get_temp::<SettingsPage>(shown_id)) != Some(self.settings_ui.page);
            egui::ScrollArea::vertical().id_salt("settings_nav_icons").auto_shrink([false, false]).show(&mut n, |ui| {
                ui.vertical_centered(|ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    for p in SettingsPage::shown() {
                        let open = self.settings_ui.page == p;
                        let r = ui_kit::icon_button(ui, p.icon(), p.title(), if open { IconState::On } else { IconState::Normal });
                        if open && reveal {
                            r.scroll_to_me(None);
                        }
                        if r.clicked() {
                            self.settings_ui.page = p;
                        }
                    }
                });
            });
            if reveal {
                ui.ctx().data_mut(|d| d.insert_temp(shown_id, self.settings_ui.page));
            }
            return;
        }
        let mut n = ui.new_child(UiBuilder::new().max_rect(nav.shrink2(Vec2::new(10.0, 16.0))).layout(Layout::top_down(Align::Min)));
        n.spacing_mut().item_spacing.y = 2.0;
        n.horizontal(|ui| {
            ui.add_space(8.0);
            ui.label(RichText::new("Configurações").font(theme::display(size::HEADING)).color(theme::GIZ));
        });
        n.add_space(space::L);
        for p in SettingsPage::shown() {
            let label = format!("Configurações: {}", p.title());
            let row = ui_kit::Row { icon: p.icon(), name: p.title(), label: &label, selected: self.settings_ui.page == p, unread: false, count: 0, mentions: 0, hover: None };
            if ui_kit::sidebar_row(&mut n, row).0.clicked() {
                self.settings_ui.page = p;
            }
        }
        // (Only where it clears the rows: a short window has none to spare.)
        if n.min_rect().bottom() < nav.bottom() - 36.0 {
            ui.painter().text(
                Pos2::new(nav.left() + 18.0, nav.bottom() - 16.0),
                Align2::LEFT_BOTTOM,
                format!("concord {}", env!("CARGO_PKG_VERSION")),
                theme::body(size::SMALL),
                theme::NEVOA,
            );
        }
    }

    /// The chosen section: its title, the ✕ and the page in a scroll area. Returns true when the
    /// ✕ was clicked.
    fn settings_page(&mut self, ui: &mut egui::Ui, content: Rect, ch: &mut Changes) -> bool {
        let page = self.settings_ui.page;
        let head = Rect::from_min_size(content.min, Vec2::new(content.width(), HEAD_H));
        ui.painter().text(Pos2::new(head.left() + 28.0, head.center().y), Align2::LEFT_CENTER, page.title(), theme::display(size::HEADING), theme::GIZ);
        ui.painter().hline(Rect::from_min_max(head.left_bottom(), head.right_bottom()).shrink2(Vec2::new(1.0, 0.0)).x_range(), head.bottom() - 0.5, Stroke::new(stroke::HAIRLINE, theme::RISCO));
        let x_rect = Rect::from_center_size(Pos2::new(head.right() - 30.0, head.center().y), Vec2::splat(30.0));
        let mut x = ui.new_child(UiBuilder::new().max_rect(x_rect).layout(Layout::left_to_right(Align::Center)));
        let close = ui_kit::icon_button(&mut x, ph::X, "Fechar (Esc)", IconState::Normal).clicked();

        let body = Rect::from_min_max(Pos2::new(content.left(), head.bottom()), content.max - Vec2::new(1.0, 1.0));
        let mut b = ui.new_child(UiBuilder::new().max_rect(body).layout(Layout::top_down(Align::Min)));
        b.set_clip_rect(body);
        egui::ScrollArea::vertical().id_salt(("settings_page", page.slug())).auto_shrink([false, false]).show(&mut b, |ui| {
            egui::Frame::new().inner_margin(Margin { left: 28, right: 24, top: 18, bottom: 28 }).show(ui, |ui| {
                ui.set_max_width(PAGE_MAX_W.min(ui.available_width()));
                forms::fit_sliders(ui);
                ui.spacing_mut().item_spacing.y = 6.0;
                match page {
                    SettingsPage::Account => self.page_account(ui),
                    SettingsPage::Voice => self.page_voice(ui, ch),
                    SettingsPage::Keybinds => ch.ptt |= self.page_keybinds(ui),
                    SettingsPage::Video => self.page_video(ui, ch),
                    SettingsPage::Notifications => self.page_notifications(ui, ch),
                    SettingsPage::Chat => self.page_chat(ui, ch),
                    SettingsPage::Sounds => ch.save |= self.soundboard_settings(ui),
                    SettingsPage::Privacy => self.page_privacy(ui),
                    SettingsPage::About => page_about(ui),
                    SettingsPage::Donate => self.page_donate(ui),
                }
                if std::mem::take(&mut self.settings_ui.scroll_end) {
                    // The screenshots' second half of a long page.
                    ui.scroll_to_cursor(Some(Align::BOTTOM));
                }
            });
        });
        close
    }

    /// After the window: the mic test, push-to-talk, saving, closing.
    fn apply_settings_changes(&mut self, ch: Changes, devices_before: (Option<String>, Option<String>), close: bool) {
        let mut save = ch.save;
        let mut ptt_changed = ch.ptt;
        if ch.gate {
            self.apply_gate_config();
            save = true;
        }
        if ch.toggle_test {
            if self.settings_ui.mic_test == MicTestUi::Off {
                self.start_mic_test();
            } else {
                self.stop_mic_test();
            }
        } else if self.settings_ui.mic_test != MicTestUi::Off && devices_before != (self.settings.input_device.clone(), self.settings.output_device.clone()) {
            // A different device was picked while testing: test that one.
            self.backend.send(Cmd::StopMicTest);
            self.start_mic_test();
        }
        if devices_before.0 != self.settings.input_device {
            self.apply_input_device();
        }
        if close {
            self.flush_camera_settings(false);
            self.stop_mic_test();
            self.settings_ui.mic_test_error = None;
            self.settings_ui.open = false;
            if self.settings_ui.capturing_key {
                self.keybinds_settings_closed();
                ptt_changed = true;
            }
        }
        if ch.test_alert {
            self.test_alert();
        }
        if ptt_changed {
            self.ptt_settings_changed();
        } else if save {
            self.save_settings();
        }
    }

    // ------------------------------------------------------------ Conta e perfil

    fn page_account(&mut self, ui: &mut egui::Ui) {
        let mut any = false;
        if self.profile_picture_section(ui) {
            any = true;
            forms::divider(ui);
        }
        if self.profile_section(ui) {
            any = true;
            forms::divider(ui);
        }
        if self.roles.supported {
            any = true;
            self.personal_password_section(ui);
        }
        if !any {
            let text = if self.me.is_none() {
                "Entre num servidor para mudar sua foto, seu perfil e sua senha pessoal: eles ficam guardados em cada servidor."
            } else {
                "Este servidor é de uma versão antiga: não tem foto de perfil, perfil nem senha pessoal."
            };
            note(ui, text, false);
        }
    }

    // ------------------------------------------------------------ Voz

    fn page_voice(&mut self, ui: &mut egui::Ui, ch: &mut Changes) {
        forms::group(ui, "Dispositivos");
        let default_in = self.settings_ui.default_input.clone();
        let default_out = self.settings_ui.default_output.clone();
        row(ui, "Microfone", |ui| device_combo(ui, "input_device", &mut self.settings.input_device, &self.settings_ui.inputs, default_in.as_deref(), &mut ch.save));
        row(ui, "Alto-falante", |ui| device_combo(ui, "output_device", &mut self.settings.output_device, &self.settings_ui.outputs, default_out.as_deref(), &mut ch.save));
        row(ui, "Volume de saída", |ui| {
            if ui.add(egui::Slider::new(&mut self.settings.volume_pct, 0..=200).suffix("%")).changed() {
                ch.save = true;
                self.backend.send(Cmd::SetVolume(self.settings.volume_pct));
            }
        });
        note(ui, "O microfone muda na hora, mesmo durante uma chamada. O alto-falante vale na próxima vez que você entrar na voz.", true);
        forms::divider(ui);
        self.input_section(ui, ch);
        forms::divider(ui);
        self.noise_section(ui, ch);
        forms::divider(ui);
        self.ptt_section(ui, ch);
        forms::divider(ui);
        self.voice_fx_section(ui, &mut ch.toggle_test);
        forms::divider(ui);
        self.alert_section(ui, ch);
    }

    /// "Testar microfone", the level meter and the input sensitivity.
    fn input_section(&mut self, ui: &mut egui::Ui, ch: &mut Changes) {
        forms::group(ui, "Teste e sensibilidade");
        let test = self.settings_ui.mic_test.clone();
        row(ui, "Teste", |ui| {
            let (icon, label) = if test == MicTestUi::Off { (ph::MICROPHONE, "Testar microfone") } else { (ph::STOP, "Parar teste") };
            if ui_kit::button(ui, Kind::Secondary, Some(icon), label).on_hover_text("Você se ouve como os outros vão ouvir (use fones para não dar eco). Nada é enviado.").clicked() {
                ch.toggle_test = true;
            }
            match &test {
                MicTestUi::Off => {}
                MicTestUi::Starting => {
                    ui.label(RichText::new("Abrindo o microfone...").color(theme::NEVOA));
                }
                MicTestUi::On(None) => {
                    ui.label(RichText::new("Fale: você se ouve como os outros vão ouvir.").color(theme::NEVOA));
                }
                MicTestUi::On(Some(w)) => {
                    ui.label(RichText::new(format!("{} {w}", ph::WARNING)).color(theme::AVISO));
                }
            }
        });
        if let Some(e) = self.settings_ui.mic_test_error.clone() {
            note_colored(ui, e, true, theme::AO_VIVO);
        }
        // The meter shows the test while it runs, else the call's own microphone.
        let source = if test != MicTestUi::Off {
            Some(self.gate_test.clone())
        } else if self.voice.channel.is_some() && !self.voice.no_mic {
            Some(self.gate_voice.clone())
        } else {
            None
        };
        forms::indented(ui, |ui| {
            let w = ui.available_width().clamp(120.0, 380.0);
            meter::level_bar(ui, source.as_deref(), Vec2::new(w, 10.0));
        });
        match (&source, &test) {
            (None, _) => note(ui, "Clique em Testar microfone para ver o nível e se ouvir. Use fones para não dar eco.", true),
            (Some(_), MicTestUi::Off) => note(ui, "Nível do seu microfone na chamada.", true),
            _ => {}
        }
        ui.add_space(space::S);
        let cfg = &mut self.settings.input_gate;
        row(ui, "Sensibilidade de entrada", |ui| {
            ch.gate |= forms::segmented(ui, &mut cfg.mode, &[(GateMode::Auto, "Automático"), (GateMode::Manual, "Manual")]);
        });
        match cfg.mode {
            GateMode::Manual => {
                row(ui, "Limite", |ui| {
                    ch.gate |= ui.add(egui::Slider::new(&mut cfg.threshold_db, THRESHOLD_MIN_DB..=THRESHOLD_MAX_DB).suffix(" dB").fixed_decimals(0)).changed();
                });
                note(ui, "A voz só passa quando o nível (a barra) fica acima do marcador branco.", true);
            }
            GateMode::Auto => {
                let text = match source.as_ref().map(|g| g.threshold_db()) {
                    Some(t) => format!("O limite acompanha o ruído do ambiente (agora {t:.0} dB, o marcador branco)."),
                    None => "O limite acompanha o ruído do ambiente.".to_string(),
                };
                note(ui, text, true);
            }
        }
        row(ui, "Manter aberto após falar", |ui| {
            ch.gate |= ui.add(egui::Slider::new(&mut cfg.hold_ms, HOLD_MIN_MS..=HOLD_MAX_MS).suffix(" ms")).changed();
        });
        if self.ptt_active() {
            note(ui, "Com push-to-talk ligado, a tecla decide e a sensibilidade não é usada.", true);
        }
    }

    fn noise_section(&mut self, ui: &mut egui::Ui, ch: &mut Changes) {
        forms::group(ui, "Supressão de ruído");
        let before = self.settings.noise_suppression;
        let options: Vec<(NoiseLevel, &str)> = NoiseLevel::ALL.iter().map(|l| (*l, l.label())).collect();
        row(ui, "Nível", |ui| forms::segmented(ui, &mut self.settings.noise_suppression, &options));
        let hint = match self.settings.noise_suppression {
            NoiseLevel::Standard => "A supressão do WebRTC: leve, tira chiado e ruído constante.",
            NoiseLevel::AiLight => "IA (RNNoise): tira também teclado, ventilador e barulho de fundo. Usa pouquíssima CPU.",
            NoiseLevel::AiMax => {
                "IA (DeepFilterNet): a mais forte; tira quase todo teclado, ventilador e rua, e abafa um pouco a conversa \
                 de fundo. Usa mais CPU (cerca de 5 a 10% de um núcleo) e soma uns 30 ms de atraso."
            }
        };
        note(ui, hint, true);
        if self.settings_ui.noise_fell_back {
            note_colored(ui, format!("{} {NOISE_FALLBACK_TEXT}", ph::WARNING), true, theme::AVISO);
        }
        if self.settings.noise_suppression != before {
            self.settings_ui.noise_fell_back = false;
            // Applies on the next 10 ms of audio: in the call and in the mic test, no rejoin.
            self.noise.set_level(self.settings.noise_suppression);
            ch.gate = true;
        }
    }

    fn ptt_section(&mut self, ui: &mut egui::Ui, ch: &mut Changes) {
        forms::group(ui, "Push-to-talk");
        if !crate::platform::global_ptt() {
            // macOS: a key that works in any app needs the Accessibility permission (phase M4).
            row(ui, "Push-to-talk", |ui| {
                let mut off = false;
                ui.add_enabled(false, egui::Checkbox::new(&mut off, "Usar push-to-talk (tecla global)")).on_disabled_hover_text(crate::platform::SOON_MAC);
            });
            note(ui, format!("Push-to-talk: {}. Use o botão do microfone ou a detecção de voz.", crate::platform::SOON_MAC_LC), true);
            if self.settings.ptt_enabled {
                // (A file from Windows: without the key the microphone would never open.)
                self.settings.ptt_enabled = false;
                ch.ptt = true;
            }
            return;
        }
        row(ui, "Push-to-talk", |ui| {
            ch.ptt |= ui.checkbox(&mut self.settings.ptt_enabled, "Usar push-to-talk (tecla global)").changed();
        });
        let keys = self.ptt_keys_text();
        let mut goto = false;
        row(ui, "Tecla", |ui| {
            ui_kit::pill(ui, &keys, theme::MOVEL, theme::GIZ);
            goto = ui_kit::button_sized(ui, Kind::Secondary, Size::Small, Some(ph::KEYBOARD), "Escolher em Atalhos").clicked();
        });
        if goto {
            self.settings_ui.page = SettingsPage::Keybinds;
        }
        if self.settings.ptt_enabled && !crate::keybinds::has_ptt(&self.settings.keybinds) {
            note_colored(ui, "Escolha uma tecla em Atalhos (\"Falar (push-to-talk)\") para o push-to-talk funcionar.", true, theme::AVISO);
        } else {
            note(ui, "Funciona com o concord em segundo plano (no jogo, por exemplo). A tecla fica em Atalhos, com os outros atalhos.", true);
        }
    }

    /// "Sons de alerta": the switch, the volume and the voice / presence ones (the chat ones are
    /// under Notificações).
    fn alert_section(&mut self, ui: &mut egui::Ui, ch: &mut Changes) {
        forms::group(ui, "Sons de alerta");
        row(ui, "Sons de alerta", |ui| {
            ch.save |= ui.checkbox(&mut self.settings.alerts.enabled, "Tocar sons de alerta (só você ouve)").changed();
        });
        ui.add_enabled_ui(self.settings.alerts.enabled, |ui| {
            row(ui, "Volume dos alertas", |ui| {
                ch.save |= ui.add(egui::Slider::new(&mut self.settings.alerts.volume_pct, 0..=100).suffix("%")).changed();
                if ui_kit::button_sized(ui, Kind::Secondary, Size::Small, Some(ph::PLAY), "Testar").clicked() {
                    ch.test_alert = true;
                }
            });
            let a = &mut self.settings.alerts;
            row(ui, "Tocar quando", |ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().interact_size.y = forms::ROW_H;
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ch.save |= ui.checkbox(&mut a.room, "Alguém entrou ou saiu da sala de voz").changed();
                    ch.save |= ui.checkbox(&mut a.streams, "Transmissões: alguém começou/parou ou entrou na sua").changed();
                    ch.save |= ui.checkbox(&mut a.own_state, "Quando eu mutar ou ensurdecer").changed();
                    ch.save |= ui.checkbox(&mut a.presence, "Alguém ficou online ou offline no servidor").changed();
                });
            });
        });
        note(ui, "Os sons de mensagens e de chamadas ficam em Notificações.", true);
    }

    // ------------------------------------------------------------ Vídeo

    fn page_video(&mut self, ui: &mut egui::Ui, ch: &mut Changes) {
        ch.save |= self.camera_section(ui);
        forms::divider(ui);
        ch.save |= self.encoder_section(ui);
        forms::divider(ui);
        forms::group(ui, "Transmitir (Go Live)");
        row(ui, "Qualidade padrão", |ui| {
            egui::ComboBox::from_id_salt("preset").width(forms::combo_w(ui)).selected_text(self.settings.preset.label()).show_ui(ui, |ui| {
                for p in Preset::ALL {
                    ch.save |= ui.selectable_value(&mut self.settings.preset, p, p.label()).changed();
                }
            });
        });
        note(ui, "Quem assiste recebe até 4, 8, 12 ou 16 Mbps (720p, 1080p, 1080p 60, Máxima). Dá para mudar na hora de transmitir.", true);
        row(ui, "Áudio", |ui| {
            ch.save |= ui.checkbox(&mut self.settings.share_audio, "Compartilhar o áudio do app por padrão").changed();
        });
        row(ui, "Prévia própria", |ui| {
            ch.save |= ui
                .checkbox(&mut self.settings.show_self_preview, "Mostrar prévia da minha transmissão")
                .on_hover_text("Desligado: sua transmissão não mostra imagem para você (só \"Ao vivo\" e quantas pessoas assistem) e a captura libera a memória da prévia, na RAM e na GPU. Quem assiste não muda.")
                .changed();
        });
        row(ui, "Prévia para os outros", |ui| {
            if ui
                .checkbox(&mut self.settings.share_hover_preview, "Prévia da minha transmissão ao passar o mouse")
                .on_hover_text("Quem passar o mouse sobre a sua transmissão vê uma imagem pequena do que você transmite, atualizada de tempos em tempos, e o nome do app ou jogo. Desligado: nenhuma imagem sai do seu computador e a que estiver no servidor é apagada.")
                .changed()
            {
                self.backend.send(Cmd::StreamPreviewUpload(self.settings.share_hover_preview));
                ch.save = true;
            }
        });
        forms::divider(ui);
        forms::group(ui, "Assistir transmissões");
        row(ui, "Janela", |ui| {
            if ui
                .checkbox(&mut self.settings.stream_popout, "Abrir transmissões em janela separada")
                .on_hover_text("Desligado: a transmissão aparece dentro do concord, na chamada.")
                .changed()
            {
                vox_client_core::golive::Viewer::set_open_inline(!self.settings.stream_popout);
                ch.save = true;
            }
        });
        row(ui, "Som", |ui| {
            ch.save |= ui.checkbox(&mut self.settings.stream_focus_audio, FOCUS_AUDIO_LABEL).on_hover_text(FOCUS_AUDIO_HOVER).changed();
        });
    }

    // ------------------------------------------------------------ Notificações

    fn page_notifications(&mut self, ui: &mut egui::Ui, ch: &mut Changes) {
        forms::group(ui, "Windows e bandeja");
        row(ui, "Notificações do Windows", |ui| {
            egui::ComboBox::from_id_salt("win_notify").width(forms::combo_w(ui)).selected_text(self.settings.notify.windows.label()).show_ui(ui, |ui| {
                for m in WinNotify::ALL {
                    ch.save |= ui.selectable_value(&mut self.settings.notify.windows, m, m.label()).changed();
                }
            });
        });
        note(ui, "Com o concord fora de foco, minimizado ou na bandeja: o botão da barra de tarefas pisca, ganha o número de não lidas e o Windows avisa quem escreveu. Clicar no aviso abre a conversa.", true);
        row(ui, "Bandeja", |ui| {
            ch.save |= ui
                .checkbox(&mut self.settings.notify.tray_on_close, "Ao fechar, minimizar para a bandeja")
                .on_hover_text("O concord continua aberto (e a chamada continua) perto do relógio. Clique no ícone para abrir; botão direito e Sair para fechar de verdade.")
                .changed();
        });
        forms::divider(ui);
        forms::group(ui, "Sons de chat");
        if !self.settings.alerts.enabled {
            let go = forms::indented(ui, |ui| {
                ui.label(RichText::new("Os sons de alerta estão desligados.").color(theme::AVISO));
                ui_kit::button_sized(ui, Kind::Ghost, Size::Small, Some(ph::MICROPHONE), "Ligar em Voz").clicked()
            });
            if go {
                self.settings_ui.page = SettingsPage::Voice;
            }
        }
        let mut listen = false;
        ui.add_enabled_ui(self.settings.alerts.enabled, |ui| {
            let a = &mut self.settings.alerts;
            row(ui, "Som dos avisos", |ui| {
                let before = a.palette;
                egui::ComboBox::from_id_salt("alert_palette").width(forms::COMBO_W.min(ui.available_width() - 96.0).max(120.0)).selected_text(alerts::palette_label(a.palette)).show_ui(ui, |ui| {
                    for (p, label) in alerts::PALETTE_CHOICES {
                        ui.selectable_value(&mut a.palette, p, label);
                    }
                });
                if a.palette != before {
                    // From the next sound on, no restart.
                    alerts::apply_palette(a);
                    ch.save = true;
                }
                listen = ui_kit::button_sized(ui, Kind::Secondary, Size::Small, Some(ph::PLAY), "Ouvir").on_hover_text("Toca alguns avisos com este som (só você ouve).").clicked();
            });
            row(ui, "Tocar quando", |ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().interact_size.y = forms::ROW_H;
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ch.save |= ui.checkbox(&mut a.dms, "Mensagem direta recebida").changed();
                    ch.save |= ui.checkbox(&mut a.chat, "Mensagem nova em outro canal").changed();
                    ch.save |= ui.checkbox(&mut a.calls, "Chamada recebida (toque)").changed();
                });
            });
        });
        note(ui, "O som escolhido vale para todos os avisos, os de voz também. O volume é o dos sons de alerta (Voz).", true);
        if listen {
            self.start_palette_preview(ui.ctx());
        }
        self.run_palette_preview(ui.ctx());
    }

    /// "Ouvir" of "Som dos avisos": [`alerts::PALETTE_PREVIEW`] through the normal alert path
    /// (voice mixer or the alert player, the alert volume and device). Like "Testar", it ignores
    /// the category switches but not "Não perturbe".
    fn start_palette_preview(&mut self, ctx: &egui::Context) {
        if self.notifications_muted() {
            self.toast("Não perturbe está ligado: os sons de alerta ficam em silêncio.");
            return;
        }
        let started = ctx.input(|i| i.time);
        ctx.data_mut(|d| d.insert_temp(palette_preview_id(), PalettePreview { started, played: 0 }));
    }

    /// Plays the preview's sounds as they come due (while Notificações is open: leaving the page
    /// stops it).
    fn run_palette_preview(&mut self, ctx: &egui::Context) {
        let Some(mut p) = ctx.data(|d| d.get_temp::<PalettePreview>(palette_preview_id())) else { return };
        let now = ctx.input(|i| i.time);
        let last = alerts::PALETTE_PREVIEW[alerts::PALETTE_PREVIEW.len() - 1].0;
        if now - p.started > last + 0.5 {
            // Left the page mid-preview and came back later: the rest is stale, not a burst.
            ctx.data_mut(|d| d.remove::<PalettePreview>(palette_preview_id()));
            return;
        }
        while let Some(&(at, kind)) = alerts::PALETTE_PREVIEW.get(p.played) {
            if now - p.started < at {
                break;
            }
            self.send_alert(kind, false);
            p.played += 1;
        }
        match alerts::PALETTE_PREVIEW.get(p.played) {
            Some(&(at, _)) => {
                ctx.data_mut(|d| d.insert_temp(palette_preview_id(), p));
                ctx.request_repaint_after(std::time::Duration::from_secs_f64((p.started + at - now).max(0.0)));
            }
            None => ctx.data_mut(|d| d.remove::<PalettePreview>(palette_preview_id())),
        }
    }

    // ------------------------------------------------------------ Chat

    fn page_chat(&mut self, ui: &mut egui::Ui, ch: &mut Changes) {
        forms::group(ui, "Mensagens");
        row(ui, "Links", |ui| {
            ch.save |= ui
                .checkbox(&mut self.settings.show_link_previews, "Mostrar prévia de links")
                .on_hover_text("Cartões com título e miniatura embaixo dos links. O servidor busca a página (você não acessa o site).")
                .changed();
        });
        row(ui, "", |ui| {
            ui.add_enabled_ui(self.settings.show_link_previews, |ui| {
                ch.save |= ui
                    .checkbox(&mut self.settings.e2e_link_previews, "Prévias de links nas conversas criptografadas")
                    .on_hover_text("Ligado, o servidor fica sabendo qual link foi mandado numa conversa criptografada.")
                    .changed();
            });
        });
        note(ui, "Ligado: o servidor fica sabendo qual link.", true);
        row(ui, "Imagens", |ui| {
            ch.save |= ui
                .checkbox(&mut self.settings.auto_show_images, "Mostrar imagens automaticamente")
                .on_hover_text("Desligado: as imagens do chat só carregam quando você clica nelas.")
                .changed();
        });
        forms::divider(ui);
        forms::group(ui, "Imagens que você envia");
        row(ui, "Qualidade", |ui| {
            let current = self.settings.image_quality;
            egui::ComboBox::from_id_salt("image_quality").width(forms::combo_w(ui)).selected_text(current.label()).show_ui(ui, |ui| {
                for (q, label) in crate::chat_images::quality_choices() {
                    ch.save |= ui.selectable_value(&mut self.settings.image_quality, q, label).changed();
                }
            });
        });
        note(ui, "As imagens são compactadas no seu PC antes de enviar (sem metadados, sem GPS). Prints com texto ficam nítidos em qualquer opção.", true);
    }
}

// ------------------------------------------------------------ Sobre

fn page_about(ui: &mut egui::Ui) {
    // v1.0.1: the concord logo and wordmark, and the signature.
    crate::brand::wordmark(ui, 34.0, 30.0);
    ui.label(RichText::new(format!("Versão {}", env!("CARGO_PKG_VERSION"))).color(theme::TEXTO));
    note(ui, "Voz, tela e chat para o seu grupo, rodando no PC de um de vocês.", false);
    ui.add_space(space::S);
    crate::brand::signature(ui, 13.0, theme::NEVOA);
    forms::divider(ui);
    forms::group(ui, "Registros");
    note(ui, "Se algo der errado, os registros (logs) ajudam a achar o problema. Eles ficam só neste computador; nada é enviado.", false);
    ui.add_space(space::S);
    let dir = LOG_DIR.get().cloned().unwrap_or_else(|| vox_log::default_log_dir("app"));
    ui.horizontal(|ui| {
        if ui_kit::button(ui, Kind::Secondary, Some(ph::FOLDER_OPEN), "Abrir pasta dos registros").clicked() {
            open_folder(&dir);
        }
    });
    ui.add(egui::Label::new(RichText::new(dir.display().to_string()).monospace().size(12.0).color(theme::NEVOA)).truncate());
    forms::divider(ui);
    forms::group(ui, "Créditos");
    note(ui, "Letras Atkinson Hyperlegible Next e Bricolage Grotesque (licença OFL), ícones Phosphor (MIT) e emojis Twemoji (CC-BY 4.0). Os avisos completos estão em THIRD-PARTY-NOTICES.", false);
}

/// Open a folder in Explorer (never in an autotest: it would take the user's foreground).
fn open_folder(dir: &std::path::Path) {
    if vox_client_core::testmode::background() {
        tracing::info!("background test mode: folder not opened");
        return;
    }
    let _ = std::fs::create_dir_all(dir);
    #[cfg(target_os = "macos")]
    crate::platform::open(dir.as_os_str());
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let op: Vec<u16> = "open\0".encode_utf16().collect();
        let target: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        // SAFETY: both strings are NUL-terminated UTF-16 buffers that outlive the call.
        let r = unsafe { ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), target.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
        // ShellExecuteW returns a value > 32 on success.
        if r as isize <= 32 {
            tracing::warn!(code = r as isize, "could not open the logs folder");
        }
    }
}

/// A device picker: "Padrão do sistema (name)" or one of the devices.
fn device_combo(ui: &mut egui::Ui, salt: &str, value: &mut Option<String>, devices: &[String], default: Option<&str>, changed: &mut bool) {
    let current = match value {
        Some(v) => v.clone(),
        None => format!("Padrão do sistema{}", default.map(|d| format!(" ({d})")).unwrap_or_default()),
    };
    egui::ComboBox::from_id_salt(salt).width(forms::combo_w(ui)).selected_text(current).show_ui(ui, |ui| {
        if ui.selectable_label(value.is_none(), "Padrão do sistema").clicked() {
            *value = None;
            *changed = true;
        }
        for d in devices {
            if ui.selectable_label(value.as_deref() == Some(d.as_str()), d).clicked() {
                *value = Some(d.clone());
                *changed = true;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_page_has_a_title_an_icon_and_a_distinct_slug() {
        let mut slugs: Vec<&str> = SettingsPage::ALL.iter().map(|p| p.slug()).collect();
        for p in SettingsPage::ALL {
            assert!(!p.title().is_empty() && !p.icon().is_empty());
            // Sentence case, never ALL CAPS.
            assert_ne!(p.title(), p.title().to_uppercase(), "{p:?}");
        }
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), SettingsPage::ALL.len());
        assert_eq!(SettingsPage::default(), SettingsPage::Account);
        assert!(!slugs.contains(&SettingsPage::Donate.slug()) && !SettingsPage::Donate.icon().is_empty());
    }

    #[test]
    fn the_donation_page_is_listed_only_with_a_pix() {
        assert_eq!(SettingsPage::shown().any(|p| p == SettingsPage::Donate), crate::donate::pix().is_some());
    }
}
