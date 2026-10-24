//! Settings → Vídeo (v0.7): hardware acceleration on/off and the video encoder used by the
//! screen share and the camera, with each detected encoder's self-test result and "Testar de
//! novo". The list and the results live in `vox_client_core::golive::encoders`.

use std::sync::Mutex;
use std::time::Duration;

use eframe::egui::{self, RichText};
use vox_client_core::golive::encoders::{self, EncoderSnapshot};
use vox_capture::EncoderKind;

use super::App;
use crate::forms;
use crate::theme::{self, ph};
use crate::ui_kit::{self, Kind, Size};

/// The settings' copy of the encoder list, refreshed when the core's generation moves.
static SNAPSHOT: Mutex<Option<EncoderSnapshot>> = Mutex::new(None);

fn snapshot() -> EncoderSnapshot {
    let mut s = SNAPSHOT.lock().unwrap_or_else(|p| p.into_inner());
    if s.as_ref().is_none_or(|x| x.generation != encoders::generation()) {
        *s = Some(encoders::snapshot());
    }
    s.clone().unwrap_or_default()
}

const AUTO: &str = "Automático (recomendado)";

impl App {
    /// Start the encoder self-test machinery and hand it the saved choice (at startup).
    pub(crate) fn init_video_encoders(&self) {
        encoders::set_preference(self.settings.encoder_pref());
        // Autotests (no persistence) test too, but keep nothing; VOX_ENCODER_SELFTEST=0 skips it.
        let cache = if self.persist { crate::settings::encoder_cache_path() } else { None };
        let test = std::env::var("VOX_ENCODER_SELFTEST").map_or(true, |v| v != "0");
        encoders::init(cache, test);
    }

    /// "Codificação de vídeo" in Configurações → Vídeo. Returns whether a setting changed.
    pub(crate) fn encoder_section(&mut self, ui: &mut egui::Ui) -> bool {
        let snap = snapshot();
        let mut changed = false;
        forms::group(ui, "Codificação de vídeo (transmissão e câmera)");
        forms::row(ui, "Aceleração de hardware", |ui| {
            changed |= ui
                .checkbox(&mut self.settings.hw_accel, "Usar a placa de vídeo")
                .on_hover_text(
                    "Ligado: a placa de vídeo codifica a transmissão e a câmera (quase não usa o processador). \
                     Desligado: o processador faz isso (mais pesado, mas funciona em qualquer PC). \
                     Vale na próxima vez que você transmitir ou ligar a câmera.",
                )
                .changed();
        });
        ui.add_enabled_ui(self.settings.hw_accel, |ui| {
            forms::row(ui, "Codificador", |ui| {
                let current = match self.settings.video_encoder.as_deref() {
                    None => AUTO.to_string(),
                    Some(id) => snap.options.iter().find(|o| o.id == id).map(|o| o.label.clone()).unwrap_or_else(|| AUTO.to_string()),
                };
                egui::ComboBox::from_id_salt("video_encoder").selected_text(current).width(forms::combo_w(ui)).show_ui(ui, |ui| {
                    let auto = format!("{AUTO} · agora: {}", crate::app::shown_encoder_label(&snap.auto_label));
                    changed |= ui.selectable_value(&mut self.settings.video_encoder, None, auto).changed();
                    for o in &snap.options {
                        let mut text = format!("{} · {}", crate::app::shown_encoder_label(&o.label), o.note);
                        if o.demoted {
                            text.push_str(" · instável agora");
                        }
                        let pick = Some(o.id.clone());
                        let resp = ui.selectable_value(&mut self.settings.video_encoder, pick, text);
                        if o.kind == EncoderKind::Software {
                            resp.clone().on_hover_text("Sem placa de vídeo: mais pesado para o processador.");
                        }
                        changed |= resp.changed();
                    }
                });
            });
            forms::indented(ui, |ui| {
                if snap.testing {
                    ui.spinner();
                    ui.label(RichText::new("Testando os codificadores…").color(theme::NEVOA));
                    ui.ctx().request_repaint_after(Duration::from_millis(300));
                } else {
                    if ui_kit::button_sized(ui, Kind::Secondary, Size::Small, Some(ph::ARROWS_CLOCKWISE), "Testar de novo")
                        .on_hover_text("Testa de novo cada placa de vídeo (leva alguns segundos, em segundo plano).")
                        .clicked()
                    {
                        encoders::retest();
                        ui.ctx().request_repaint_after(Duration::from_millis(100));
                    }
                    ui.label(RichText::new(format!("Automático usa: {}", crate::app::shown_encoder_label(&snap.auto_label))).size(theme::size::SMALL + 0.5).color(theme::NEVOA));
                }
            });
        });
        if changed {
            encoders::set_preference(self.settings.encoder_pref());
        }
        changed
    }
}
