//! v1.1 (E0): Configurações → "Privacidade" → "Seus aparelhos" (a child module of `app`).
//!
//! - The devices of my account on this server's list of encryption keys: the computer's name,
//!   "este computador", when it was added, and "Remover" for the others (`Cmd::E2ee`).
//! - A device this computer never saw appearing under my account is an alert (the notice bar):
//!   "Um aparelho novo foi adicionado à sua conta: {label}. Se não foi você, remova e avise quem
//!   hospeda." It stays marked "novo" in the list for the session.
//! - What the backend says about this computer (`backend/e2ee.rs`): registering, no key storage
//!   ("Não foi possível guardar a chave de criptografia neste computador"), too many devices,
//!   removed, an older server.

use std::collections::HashSet;

use eframe::egui::{self, Align, Layout, RichText};

use super::App;
use crate::backend::e2ee::{E2eeCmd, E2eeEv, E2eeStatus, E2eeView};
use crate::backend::Cmd;
use crate::forms::{self, note};
use crate::theme::{self, ph, space};
use crate::ui_kit::{self, Kind, Size};

pub const PAGE_TITLE: &str = "Privacidade";
pub const DEVICES_TITLE: &str = "Seus aparelhos";
pub const THIS_COMPUTER: &str = "este computador";
pub const REMOVE: &str = "Remover";

/// The alert about a new device of my own account.
pub fn new_device_alert(label: &str) -> String {
    format!("Um aparelho novo foi adicionado à sua conta: {label}. Se não foi você, remova e avise quem hospeda.")
}

/// The page's state (what the backend last said).
#[derive(Debug, Default)]
pub struct PrivacyUi {
    pub view: E2eeView,
    /// Devices of mine announced as new during this session (marked "novo").
    pub new_devices: HashSet<String>,
    /// Alerts shown this session (the autotest reads them).
    pub alerts: Vec<String>,
}

impl App {
    /// The backend's news about this computer's identity and my devices.
    pub(crate) fn on_e2ee_event(&mut self, ev: E2eeEv) {
        match ev {
            E2eeEv::View(v) => {
                if v.status == E2eeStatus::Off {
                    self.privacy.new_devices.clear();
                }
                self.privacy.view = v;
            }
            E2eeEv::NewOwnDevice { device_id, label } => {
                let text = new_device_alert(&label);
                tracing::info!("e2ee: alert shown: a new device of my account");
                self.privacy.new_devices.insert(device_id);
                self.privacy.alerts.push(text.clone());
                self.notice = Some(format!("{} {text}", ph::SHIELD));
            }
            // E2: local lines in the conversation with them (and, when the code had been
            // verified, the DM's "Verifique de novo" banner, from the view).
            E2eeEv::PeerKeys { user_id, added, was_verified } => {
                tracing::info!(user = user_id, added = added.len(), was_verified, "e2ee: local lines about another member's keys");
                self.dm_peer_keys(user_id, &added);
            }
        }
    }

    /// "Remover" on one of my other devices.
    pub(crate) fn remove_e2ee_device(&mut self, device_id: &str) {
        self.backend.send(Cmd::E2ee(E2eeCmd::Remove(device_id.to_string())));
    }

    /// Configurações → Privacidade.
    pub(crate) fn page_privacy(&mut self, ui: &mut egui::Ui) {
        forms::group_icon(ui, ph::SHIELD, DEVICES_TITLE);
        note(ui, "Cada computador em que você entra neste servidor tem uma chave de criptografia própria. Se aparecer um aparelho que você não reconhece, remova e avise quem hospeda.", false);
        ui.add_space(space::S);
        let v = self.privacy.view.clone();
        let status = match v.status {
            E2eeStatus::Off => Some((ui_kit::Notice::Info, "Entre em um servidor para ver os seus aparelhos.".to_string())),
            E2eeStatus::Unsupported => Some((ui_kit::Notice::Info, "Este servidor ainda não tem criptografia de ponta a ponta (servidor antigo).".to_string())),
            E2eeStatus::Starting => Some((ui_kit::Notice::Info, "Registrando este computador…".to_string())),
            E2eeStatus::NoKeyStorage => Some((ui_kit::Notice::Error, format!("{}.", crate::e2ee_store::UNAVAILABLE_TEXT))),
            E2eeStatus::Removed => Some((ui_kit::Notice::Warning, "Este computador foi removido da sua conta. Na próxima vez que você entrar, ele volta como um aparelho novo.".to_string())),
            E2eeStatus::TooMany | E2eeStatus::Failed => v.error.clone().map(|e| (ui_kit::Notice::Warning, e)),
            E2eeStatus::Active => None,
        };
        if let Some((kind, text)) = status {
            ui_kit::notice_frame(kind).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui_kit::notice_text(ui, kind, &text);
            });
            ui.add_space(space::S);
        }
        if v.status == E2eeStatus::Active {
            if let Some(e) = &v.error {
                ui_kit::notice_frame(ui_kit::Notice::Warning).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui_kit::notice_text(ui, ui_kit::Notice::Warning, e);
                });
                ui.add_space(space::S);
            }
        }
        if v.mine.is_empty() {
            if matches!(v.status, E2eeStatus::Active | E2eeStatus::Removed | E2eeStatus::NoKeyStorage) {
                note(ui, "Nenhum aparelho com criptografia nesta conta.", false);
            }
            return;
        }
        let offset = crate::app::local_offset_min();
        let mut remove = None;
        for d in &v.mine {
            let this = v.this_device.as_deref() == Some(d.device_id.as_str());
            let fresh = self.privacy.new_devices.contains(&d.device_id);
            egui::Frame::new().fill(theme::PAREDE).stroke(egui::Stroke::new(1.0, if fresh { theme::ABAJUR } else { theme::RISCO })).corner_radius(theme::radius::CARD).inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(if this { ph::MONITOR } else { ph::HARD_DRIVES }).size(20.0).color(theme::NEVOA));
                    ui.add_space(space::S);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.horizontal(|ui| {
                            ui.add(egui::Label::new(RichText::new(&d.label).font(theme::semibold(theme::size::BODY)).color(theme::GIZ)).truncate());
                            if this {
                                ui_kit::pill(ui, THIS_COMPUTER, theme::ABAJUR_FUNDO, theme::ABAJUR);
                            }
                            if fresh {
                                ui_kit::pill(ui, "novo", theme::PERIGO_FUNDO, theme::AO_VIVO);
                            }
                        });
                        ui.label(RichText::new(format!("Adicionado em {}", crate::profile::format_date_pt(d.created_at_ms, offset))).font(theme::body(theme::size::SMALL)).color(theme::NEVOA));
                    });
                    if !this {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if v.removing.as_deref() == Some(d.device_id.as_str()) {
                                ui.add(egui::Spinner::new().size(16.0));
                                ui.label(RichText::new("Removendo…").font(theme::body(theme::size::SMALL)).color(theme::NEVOA));
                            } else if ui_kit::button_sized(ui, Kind::Danger, Size::Small, Some(ph::TRASH), REMOVE).on_hover_text("Tira este aparelho da sua conta: ele deixa de receber as suas mensagens criptografadas.").clicked() {
                                remove = Some(d.device_id.clone());
                            }
                        });
                    }
                });
            });
            ui.add_space(space::XS);
        }
        ui.add_space(space::S);
        note(ui, format!("Até {} aparelhos por conta. Remover um aparelho não desconecta quem já entrou: se ele não é seu, avise quem hospeda e troque a sua senha pessoal.", vox_proto::E2EE_MAX_DEVICES), false);
        if let Some(id) = remove {
            self.remove_e2ee_device(&id);
        }
    }
}
