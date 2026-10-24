//! "Apoie o concord" (v1.1, D1), on screen (a child module of `app`): the popup, at most twice per
//! user, and the page "Apoiar o projeto" of Configurações. The Pix and the rules are in `donate.rs`.
//! Nothing here runs while no Pix is set, in an autotest (unless the `donate` role asks) or in the
//! background test mode.

use std::time::Instant;

use eframe::egui::emath::GuiRounding as _;
use eframe::egui::{self, Align, Align2, Color32, ColorImage, Layout, Rect, Sense, TextureHandle, TextureOptions, Vec2};

use super::App;
use crate::donate::{self, CalmGate, PixInfo, UsageClock};
use crate::forms;
use crate::theme::{ph, radius, space};
use crate::ui_kit::{self, Kind};

pub const PAGE_TITLE: &str = "Apoiar o projeto";
const POPUP_TITLE: &str = "Apoie o concord";
const TEXT: &str = "O concord é gratuito e feito por uma pessoa só. Se ele te ajuda, um Pix de qualquer valor ajuda a manter o projeto.";
const COPY_KEY: &str = "Copiar chave Pix";
const COPY_CODE: &str = "Pix copia e cola";

/// The QR code's side in the popup: as much as the window lets it have (phones read 150 well).
const QR_POPUP: (f32, f32) = (150.0, 220.0);
/// In Configurações.
const QR_PAGE: f32 = 220.0;
/// Over the QR code in the popup's scroll: the text and the gap under it.
const TEXT_H: f32 = 80.0;
/// White modules around the code: readers need this "quiet zone", and the page is dark.
const QUIET: usize = 4;

pub struct DonateUi {
    /// The popups may happen in this run: never in a test, unless the `donate` role asks.
    pub enabled: bool,
    /// The popup is up (already counted in the settings).
    pub open: bool,
    /// The QR code: made once, the first time it is needed.
    qr: Option<TextureHandle>,
    pub clock: UsageClock,
    calm: CalmGate,
    /// `donate` autotest: what would have gone to the clipboard (a test leaves the user's alone).
    #[cfg(feature = "donate-test")]
    pub copied: Option<String>,
}

impl DonateUi {
    pub fn new(persist: bool) -> Self {
        DonateUi {
            enabled: persist && !vox_client_core::testmode::background(),
            open: false,
            qr: None,
            clock: UsageClock::default(),
            calm: CalmGate::default(),
            #[cfg(feature = "donate-test")]
            copied: None,
        }
    }
}

/// The code as a texture: one texel per module, the quiet zone included, drawn with no smoothing.
fn qr_texture(ctx: &egui::Context, pix: &PixInfo) -> Option<TextureHandle> {
    let (side, modules) = donate::qr_modules(&donate::br_code(pix))?;
    let full = side + 2 * QUIET;
    let mut image = ColorImage::filled([full, full], Color32::WHITE);
    for (i, _) in modules.iter().enumerate().filter(|(_, dark)| **dark) {
        image[(i % side + QUIET, i / side + QUIET)] = Color32::BLACK;
    }
    Some(ctx.load_texture("pix_qr", image, TextureOptions::NEAREST))
}

impl App {
    /// From `logic()` (also in the tray): the usage counter. No wake-ups once nothing is owed.
    pub(super) fn donate_tick(&mut self, ctx: &egui::Context) {
        if self.donate.open && self.me.is_none() {
            self.donate.open = false;
        }
        if !self.donate.enabled || self.settings.donate_popups_shown >= donate::MAX_POPUPS || donate::pix().is_none() {
            return;
        }
        let now = Instant::now();
        self.donate_count(now, self.me.is_some());
        if let Some(wait) = self.donate.clock.next(now) {
            ctx.request_repaint_after(wait);
        }
    }

    pub(super) fn donate_on_exit(&mut self) {
        if self.donate.enabled && donate::pix().is_some() {
            self.donate_count(Instant::now(), false);
        }
    }

    fn donate_count(&mut self, now: Instant, counting: bool) {
        let counted = self.donate.clock.tick(now, counting);
        self.settings.usage_secs = self.settings.usage_secs.saturating_add(counted.secs);
        if counted.save {
            self.save_settings();
        }
    }

    /// No call, no dialog, and the person is looking at the window. (The `egui` layers above the
    /// main view are every dialog, panel and menu there is, today's and tomorrow's.)
    fn donate_calm(&self, ctx: &egui::Context) -> bool {
        let call = self.voice.channel.is_some() || self.voice.joining.is_some() || self.dms.call.is_some();
        let media = self.my_share_live() || self.camera_on() || !self.voice.watching.is_empty();
        if call || media || self.notify.hidden || super::window_minimized(ctx) || !self.window_focused() || egui::Popup::is_any_open(ctx) {
            return false;
        }
        !ctx.memory(|m| m.areas().visible_layer_ids().iter().any(|l| matches!(l.order, egui::Order::Middle | egui::Order::Foreground | egui::Order::Tooltip)))
    }

    /// Open the popup when one is owed and the moment is calm.
    fn donate_maybe_open(&mut self, ctx: &egui::Context) {
        if !self.donate.enabled || donate::pix().is_none() {
            return;
        }
        let logged_in = self.ready && self.me.is_some();
        if !donate::popup_due(self.settings.donate_popups_shown, self.settings.usage_secs, logged_in) {
            return;
        }
        let calm = self.donate_calm(ctx);
        if self.donate.calm.ready(calm, Instant::now()) {
            donate::mark_shown(&mut self.settings.donate_popups_shown, &mut self.settings.usage_secs);
            self.save_settings();
            self.donate.open = true;
            tracing::info!(shown = self.settings.donate_popups_shown, "donation popup");
        } else if calm {
            ctx.request_repaint_after_secs(donate::CALM_SECS);
        }
    }

    /// The popup, once it is open (from `ui()`, so never with the window in the tray).
    pub(super) fn donate_window(&mut self, ctx: &egui::Context) {
        if !self.donate.open {
            self.donate_maybe_open(ctx);
        }
        let Some(pix) = donate::pix().filter(|_| self.donate.open) else { return };
        let mut close = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let mut copy = false;
        let room = ctx.content_rect().height() - ui_kit::DIALOG_RESERVE;
        egui::Window::new(POPUP_TITLE)
            .id(egui::Id::new("donate_popup"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(ui_kit::dialog_frame(&ctx.global_style()))
            .show(ctx, |ui| {
                ui.set_width(ui_kit::dialog_w(ctx, ui_kit::DIALOG_W));
                ui.spacing_mut().item_spacing.y = 6.0;
                ui_kit::dialog_title(ui, POPUP_TITLE);
                ui.add_space(space::S);
                ui_kit::dialog_scroll(ui, "donate_body", ui_kit::DIALOG_RESERVE, |ui| {
                    ui_kit::body_text(ui, TEXT);
                    ui.add_space(space::S);
                    ui.vertical_centered(|ui| self.donate_qr(ui, &pix, (room - TEXT_H).clamp(QR_POPUP.0, QR_POPUP.1)));
                });
                ui.add_space(space::L);
                ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), forms::ROW_H), Layout::right_to_left(Align::Center), |ui| {
                    copy = ui_kit::button(ui, Kind::Primary, Some(ph::COPY), COPY_KEY).clicked();
                    close |= ui_kit::button(ui, Kind::Secondary, None, "Agora não").clicked();
                });
            });
        if copy {
            self.donate_copy(ctx, pix.key.to_string(), "Chave Pix copiada.");
        }
        if close {
            self.donate.open = false;
        }
    }

    /// The QR code, at most `side` points wide: a whole number of screen pixels per module, on the
    /// pixel grid, or a phone sees a blurred code.
    fn donate_qr(&mut self, ui: &mut egui::Ui, pix: &PixInfo, side: f32) {
        if self.donate.qr.is_none() {
            self.donate.qr = qr_texture(ui.ctx(), pix);
        }
        let Some(tex) = &self.donate.qr else { return };
        let ppp = ui.ctx().pixels_per_point();
        let texels = tex.size()[0] as f32;
        let side = ((side * ppp / texels).floor().max(1.0) * texels / ppp).min(ui.available_width());
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(side), Sense::hover());
        let rect = Rect::from_min_size(rect.min.round_to_pixels(ppp), rect.size());
        egui::Image::new((tex.id(), rect.size())).corner_radius(radius::CARD).alt_text("QR code do Pix").paint_at(ui, rect);
    }

    /// To the clipboard, through egui like every other "Copiar" (never in a test: a test does not
    /// change the user's clipboard).
    pub fn donate_copy(&mut self, ctx: &egui::Context, text: String, done: &str) {
        if vox_client_core::testmode::background() {
            #[cfg(feature = "donate-test")]
            {
                self.donate.copied = Some(text);
            }
            tracing::info!("background test mode: the clipboard is left alone");
        } else {
            ctx.copy_text(text);
        }
        self.toast(done);
    }

    /// Configurações → "Apoiar o projeto".
    pub(super) fn page_donate(&mut self, ui: &mut egui::Ui) {
        let Some(pix) = donate::pix() else { return };
        ui_kit::body_text(ui, TEXT);
        ui.add_space(space::M);
        self.donate_qr(ui, &pix, QR_PAGE);
        ui.add_space(space::M);
        let (key, code) = ui.horizontal(|ui| (ui_kit::button(ui, Kind::Primary, Some(ph::COPY), COPY_KEY).clicked(), ui_kit::button(ui, Kind::Secondary, Some(ph::CLIPBOARD_TEXT), COPY_CODE).clicked())).inner;
        if key {
            self.donate_copy(ui.ctx(), pix.key.to_string(), "Chave Pix copiada.");
        }
        if code {
            self.donate_copy(ui.ctx(), donate::br_code(&pix), "Pix copia e cola copiado.");
        }
    }
}
