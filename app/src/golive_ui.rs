//! The Go Live window (v1.0, phase 3a): what to stream ("Tela + áudio" or "Só áudio"), the grid
//! of thumbnail cards ([`crate::sources`]), the quality and the button that goes live. A child
//! module of `app` (it uses the app's private helpers).
//!
//! Drawn as a [`forms::Dialog`] (no egui title bar). The picker's thumbnails live only while the
//! window is open: closing it, by any path, releases them and their textures.

use std::sync::Arc;

use eframe::egui::{self, Vec2};

use super::{App, Pick, Share, ShareMode, ICON_LIVE};
use crate::backend::Cmd;
use crate::forms::{self, note, note_colored};
use crate::settings::Preset;
use crate::theme::{self, ph, space};
use crate::ui_kit::{self, Kind};

/// The window's size (clamped to the app's window by the dialog).
const SIZE: Vec2 = Vec2::new(820.0, 640.0);
/// What the window takes besides the grid (the header and the mode row above it, the options and
/// the footer under it), so the grid gets the rest. (v1.0 phase 4: was 268, the part under the grid
/// only, so a full grid or a short window pushed the footer out of the window.)
const BESIDE_GRID: f32 = 416.0;
/// The footer's height (its hairline, spacing and buttons), kept out of the scroll.
const FOOTER_H: f32 = 64.0;

/// macOS: the Screen Recording permission is off. Shows what to do and a button to System
/// Settings; returns whether it is off. Nothing (and `false`) elsewhere.
fn screen_permission_note(ui: &mut egui::Ui) -> bool {
    if vox_capture::screen_capture_permission() == vox_capture::Permission::Granted {
        return false;
    }
    note_colored(
        ui,
        format!(
            "{} O macOS ainda não deixou o concord gravar a tela. Ligue o concord em Ajustes do Sistema > Privacidade e Segurança > Gravação de Tela e Áudio do Sistema e depois feche e abra o concord de novo.",
            ph::WARNING
        ),
        false,
        theme::AVISO,
    );
    if ui_kit::button(ui, Kind::Secondary, Some(ph::GEAR_SIX), "Abrir Ajustes do Sistema").clicked() {
        // The first time this shows macOS's own prompt; afterwards the app is listed, switched off.
        vox_capture::request_screen_capture_permission();
        if let Some(url) = vox_capture::privacy_settings_url(false) {
            if let Err(e) = std::process::Command::new("open").arg(url).spawn() {
                tracing::warn!(error = %e, "cannot open System Settings");
            }
        }
    }
    ui.add_space(space::S);
    true
}

impl App {
    pub(super) fn golive_window(&mut self, ctx: &egui::Context) {
        if !self.golive.open {
            // Closed (by any path): free the thumbnails and their textures.
            if !self.golive.picker.is_idle() {
                self.golive.picker.release();
                ctx.request_repaint(); // one more frame so egui frees the textures
            }
            return;
        }
        if self.golive.switching && !matches!(self.voice.share, Share::Live { .. }) {
            // The live ended while the picker was open.
            self.golive.open = false;
            self.golive.switching = false;
            self.golive.picker.release();
            ctx.request_repaint();
            return;
        }
        let mut start: Option<(Pick, Preset, bool)> = None;
        let mut refresh = false;
        let mut cancel = false;
        let switching = self.golive.switching;
        // The kind of the running live: choosing the same kind (and quality) only changes the
        // source; anything else restarts the live (viewers stay in the room and follow).
        let live_mode = if self.voice.share_audio_only { ShareMode::AudioOnly } else { ShareMode::ScreenAudio };
        let title = if switching { "Trocar o que você transmite" } else { "Transmitir" };
        let current = self.voice.share_pick.clone();
        let screen = ctx.content_rect();
        let size = SIZE.min(screen.size() - Vec2::splat(24.0));
        let grid_h = (size.y - BESIDE_GRID).clamp(150.0, 430.0);
        let open = forms::Dialog::new("golive_window", title).icon(ph::SCREENCAST).size(size).show(ctx, |ui| {
            // I6: everything above the footer scrolls when the window is short (a small or
            // zoomed-in window): the footer's buttons always show.
            let body_h = (ui.available_height() - FOOTER_H).max(80.0);
            let switch_in_place = egui::ScrollArea::vertical().id_salt("golive_body").max_height(body_h).auto_shrink([false, true]).show(ui, |ui| {
                let mut mode = self.golive.mode;
                ui.horizontal_wrapped(|ui| {
                    forms::segmented(ui, &mut mode, &[(ShareMode::ScreenAudio, "Tela + áudio"), (ShareMode::AudioOnly, "Só áudio")]);
                    ui.add_space(space::M);
                    let hint = if mode == ShareMode::AudioOnly {
                        "Só o som de um app, sem imagem: bom para ouvir música juntos."
                    } else {
                        "Uma janela ou uma tela inteira, com o som do app."
                    };
                    ui.label(egui::RichText::new(hint).color(theme::NEVOA).size(12.5));
                });
                if mode != self.golive.mode {
                    self.golive.mode = mode;
                    self.golive.pick = None;
                    if !self.golive.loading || !self.golive.windows.is_empty() {
                        // Same lists, other cards: icons of the apps, or pictures of the windows.
                        let repaint = self.repaint.clone();
                        self.golive.picker.sources_listed(&self.golive.windows, &self.golive.monitors, Arc::new(move || repaint.wake()), mode == ShareMode::AudioOnly);
                    }
                }
                let audio_only = mode == ShareMode::AudioOnly;
                // Same kind and quality as the running live: a plain source switch.
                let switch_in_place = switching && mode == live_mode && (!audio_only || self.golive.audio_high == self.voice.share_audio_high);
                ui.add_space(space::M);
                // macOS: without Screen Recording there is nothing to list (screens, windows and an
                // app's sound all need it). Say so plainly, with the way to System Settings.
                let blocked = screen_permission_note(ui);
                if let Some(e) = self.golive.error.as_ref().filter(|_| !blocked) {
                    note_colored(ui, format!("{} {e}", ph::WARNING), false, theme::AO_VIVO);
                }
                let out = crate::sources::show(
                    ui,
                    &mut self.golive.picker,
                    crate::sources::GridInput {
                        windows: &self.golive.windows,
                        monitors: &self.golive.monitors,
                        loading: self.golive.loading,
                        picked: self.golive.pick.as_ref(),
                        current: if switching && mode == live_mode { current.as_ref() } else { None },
                        max_height: grid_h,
                        audio: audio_only,
                    },
                );
                if let Some(p) = out.picked {
                    self.golive.pick = Some(p);
                }
                if out.go {
                    start = self.golive.pick.clone().map(|p| (p, self.golive.preset, self.golive.audio));
                }
                forms::divider(ui);
                if audio_only {
                    forms::row(ui, "Qualidade do áudio", |ui| {
                        forms::segmented(ui, &mut self.golive.audio_high, &[(false, "Normal (128 kbps)"), (true, "Alta (256 kbps, estéreo)")]);
                    });
                    let q = if self.golive.audio_high {
                        "Estéreo com mais detalhe. Cada pessoa que ouve recebe uma cópia, então pesa mais no seu envio."
                    } else {
                        "Estéreo, boa para música e leve para a conexão."
                    };
                    note(ui, q, true);
                    note(
                        ui,
                        "Só o som vai para a sala: nada de imagem, e sem supressão de ruído nem corte de silêncio. Quem está na sala clica em \"Ouvir\". Você não escuta a sua própria transmissão.",
                        true,
                    );
                } else {
                    if switch_in_place {
                        note(ui, "A qualidade e o modo de conexão continuam como estão. Quem assiste não precisa fazer nada.", false);
                    } else {
                        forms::row(ui, "Qualidade", |ui| {
                            let options: Vec<(Preset, &str)> = Preset::ALL.iter().map(|p| (*p, p.label())).collect();
                            forms::segmented(ui, &mut self.golive.preset, &options);
                        });
                        note(ui, format!("Até {} Mbps de vídeo. O servidor envia uma cópia para cada pessoa que assiste.", self.golive.preset.bitrate() / 1_000_000), true);
                        forms::row(ui, "Se a conexão apertar", |ui| {
                            forms::segmented(ui, &mut self.golive.smooth, &[(false, "Priorizar nitidez"), (true, "Priorizar fluidez")]);
                        });
                        let s = if self.golive.smooth {
                            "Mantém os quadros por segundo e aceita uma imagem mais borrada. Bom para jogos rápidos."
                        } else {
                            "Reduz os quadros por segundo (de 60 para 30, depois 20) para manter cada quadro nítido. Bom para texto, código e detalhes."
                        };
                        note(ui, s, true);
                    }
                    forms::row(ui, "Áudio", |ui| {
                        ui.checkbox(&mut self.golive.audio, "Compartilhar o áudio do app");
                    });
                    note(ui, "Janela: só o som daquele app. Tela inteira: o som do PC, menos o do concord.", true);
                }
                if switching && !switch_in_place {
                    note_colored(
                        ui,
                        format!("{} Mudar o tipo (tela ou só áudio) ou a qualidade do áudio reinicia a transmissão por um instante. Quem assiste continua na sala.", ph::WARNING),
                        false,
                        theme::AVISO,
                    );
                }
                switch_in_place
            })
            .inner;
            forms::footer(ui, |ui| {
                let can = self.golive.pick.is_some();
                let go = if switch_in_place {
                    "Trocar"
                } else if switching {
                    "Trocar e reiniciar"
                } else {
                    "Transmitir"
                };
                let icon = if switching { ph::ARROWS_LEFT_RIGHT } else { ICON_LIVE };
                let r = ui.add_enabled_ui(can, |ui| ui_kit::button(ui, Kind::Primary, Some(icon), go)).inner;
                if r.on_disabled_hover_text("Escolha uma janela, uma tela ou um app acima.").clicked() {
                    start = self.golive.pick.clone().map(|p| (p, self.golive.preset, self.golive.audio));
                }
                if ui_kit::button(ui, Kind::Ghost, None, "Cancelar").clicked() {
                    cancel = true;
                }
                if ui_kit::button(ui, Kind::Secondary, Some(ph::ARROWS_CLOCKWISE), "Atualizar lista").clicked() {
                    refresh = true;
                }
            });
        });
        if !open || cancel {
            self.golive.open = false;
        }
        if refresh {
            self.golive.loading = true;
            self.backend.send(Cmd::ListSources);
        }
        if let Some((pick, preset, audio)) = start {
            let audio_only = self.golive.mode == ShareMode::AudioOnly;
            let same = self.golive.mode == live_mode && (!audio_only || self.golive.audio_high == self.voice.share_audio_high);
            if switching && same {
                self.switch_share(pick, audio);
            } else {
                // A new live, or the running one becoming another kind (the core replaces it).
                self.start_share(pick, preset, audio);
            }
        }
        if !self.golive.open {
            self.golive.picker.release();
            ctx.request_repaint();
        }
    }
}
