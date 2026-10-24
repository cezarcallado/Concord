//! v1.1 (T0): the connection's lock and its dialogs (a child module of `app`).
//!
//! - The server header shows a closed lock ("Conexão protegida") while the session is pinned TLS,
//!   an open one ("Conexão sem proteção (servidor antigo)") while it is plain HTTP.
//! - "Segurança da conexão" (the server menu): the lock, and the "Código do servidor" (Crockford
//!   base32 of the first 10 bytes of the certificate pin), the same the server's console and the
//!   host panel show, for people to compare.
//! - "A identidade do servidor mudou": the saved / invited certificate is not the one that
//!   answered (nothing was sent). Cancelar / Usar um convite novo / Confiar no novo código, the
//!   last one only after a second confirmation showing both codes.
//! - The plain-HTTP prompt of a typed address without TLS (rule 4, `backend/tls.rs`).

use eframe::egui::{self, Align2, Color32, RichText, Vec2};
use vox_client_core::tls::Pin;

use crate::app::App;
use crate::backend::{PinSource, TlsPlan};
use crate::theme::{self, ph, space};
use crate::ui_kit::{self, Kind, Size};

pub const LOCK_ON: &str = "Conexão protegida";
pub const LOCK_OFF: &str = "Conexão sem proteção (servidor antigo)";
pub const SECURITY_TITLE: &str = "Segurança da conexão";
pub const MISMATCH_TITLE: &str = "A identidade do servidor mudou";
pub const MISMATCH_TEXT: &str = "Se quem hospeda reinstalou o servidor, peça um convite novo. Se não, alguém pode estar interceptando a conexão.";
pub const OLD_APPS: &str = "Aceitar apps antigos sem conexão protegida";
const PLAIN_TITLE: &str = "Conexão sem proteção";
const PLAIN_YES: &str = "Conectar sem proteção";
const NEW_INVITE: &str = "Usar um convite novo";
const TRUST_NEW: &str = "Confiar no novo código";

/// "Código do servidor: K7QM-2D9X-4HVA-PB1C".
pub fn code_line(pin: &Pin) -> String {
    format!("Código do servidor: {}", vox_proto::tls_server_code(pin))
}

/// The connection's state and dialogs.
#[derive(Debug, Default)]
pub struct TlsUi {
    /// The session's certificate (`None`: plain HTTP, or not logged in).
    pub session: Option<Pin>,
    /// "Segurança da conexão" is open.
    pub security_open: bool,
    /// "A identidade do servidor mudou" is up.
    pub mismatch: Option<Mismatch>,
    /// The plain-HTTP prompt is up for this address.
    pub plain_prompt: Option<String>,
    /// The address the user agreed to reach without TLS (asked once).
    pub plain_ok_for: Option<String>,
    /// "Confiar no novo código": the next login's pin.
    pub trust_override: Option<Pin>,
}

#[derive(Debug, Clone)]
pub struct Mismatch {
    pub url: String,
    pub user: String,
    pub expected: Pin,
    pub got: Pin,
    /// The second step (both codes) is showing.
    pub confirming: bool,
}

impl App {
    /// What a login at `typed` knows about the certificate (the trust rules, `backend/tls.rs`):
    /// "Confiar no novo código", else the saved entry's, else the saved login token's, else my own
    /// hosted server's. `saved`: the address is a saved one (entry or token).
    pub(crate) fn tls_plan(&mut self, typed: &str, entry_pin: Option<Pin>, token_pin: Option<Pin>, saved: bool) -> TlsPlan {
        let hosted = self.own_hosted_pin(typed);
        let pin = self.tls.trust_override.take().or(entry_pin).or(token_pin).or(hosted);
        let source = if saved || hosted.is_some() { PinSource::Saved } else { PinSource::Manual };
        let plain_ok = self.tls.plain_ok_for.as_deref().is_some_and(|u| crate::backend::normalize_url(u).eq_ignore_ascii_case(typed));
        TlsPlan { pin, source, plain_ok }
    }

    /// The certificate of the server I host, when `url` is its loopback address.
    fn own_hosted_pin(&self, url: &str) -> Option<Pin> {
        let st = self.host.status()?;
        let port = st.hub_addr?.port();
        let (h, p) = crate::host::url_host_port(url)?;
        let loopback = h.eq_ignore_ascii_case("localhost") || h.trim_matches(|c| c == '[' || c == ']').parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
        (loopback && p == Some(port)).then_some(st.tls_pin).flatten()
    }

    /// The lock of the server header: icon, text, colour (`None` before a login).
    pub(crate) fn tls_lock(&self) -> Option<(&'static str, &'static str, Color32)> {
        self.me.as_ref()?;
        Some(match self.tls.session {
            Some(_) => (ph::LOCK_SIMPLE, LOCK_ON, theme::NEVOA),
            None => (ph::LOCK_SIMPLE_OPEN, LOCK_OFF, theme::ABAJUR),
        })
    }

    /// The backend refused a certificate (nothing was sent).
    pub(crate) fn on_tls_mismatch(&mut self, url: String, expected: Pin, got: Pin) {
        tracing::warn!(%url, saved = %vox_proto::tls_server_code(&expected), answered = %vox_proto::tls_server_code(&got), "tls: server identity changed; asking the user");
        self.form.busy = false;
        self.servers_login_failed();
        let user = self.form.user.trim().to_string();
        self.tls.mismatch = Some(Mismatch { url, user, expected, got, confirming: false });
    }

    /// A typed address without TLS: ask before plain HTTP.
    pub(crate) fn on_plain_prompt(&mut self, url: String) {
        tracing::info!(%url, "tls: this server has no protected connection; asking the user");
        self.form.busy = false;
        self.servers_login_failed();
        self.tls.plain_prompt = Some(url);
    }

    /// "Conectar sem proteção".
    pub fn tls_accept_plain(&mut self) {
        if let Some(url) = self.tls.plain_prompt.take() {
            tracing::info!(%url, "tls: the user chose to connect without protection");
            self.tls.plain_ok_for = Some(url);
            self.connect();
        }
    }

    /// "Usar um convite novo": the form waits for a code.
    pub fn tls_use_new_invite(&mut self) {
        self.tls.mismatch = None;
        self.servers.joining = None;
        self.servers.form_open = true;
        self.form.url.clear();
        self.form.error = Some("Cole aqui o convite novo de quem hospeda o servidor (começa com vox1-).".into());
    }

    /// "Confiar no novo código" (after the second confirmation): the saved entry and login tokens
    /// of this address take the new certificate, and the login runs again with it.
    pub fn tls_trust_new(&mut self) {
        let Some(m) = self.tls.mismatch.take() else { return };
        tracing::warn!(url = %m.url, new = %vox_proto::tls_server_code(&m.got), "tls: the user trusts the new server code");
        self.settings.trust_new_pin(&m.url, &m.user, m.got);
        self.save_settings();
        self.tls.trust_override = Some(m.got);
        self.connect();
    }

    pub(crate) fn tls_windows(&mut self, ctx: &egui::Context) {
        if let Some(url) = self.tls.plain_prompt.clone() {
            let answer = ui_kit::confirm(ctx, "tls_plain", PLAIN_TITLE, PLAIN_YES, Kind::Danger, true, |ui| {
                ui_kit::body_text(ui, crate::backend::tls::PLAIN_PROMPT);
                ui_kit::hint(ui, format!("Endereço: {url}"));
            });
            match answer {
                Some(true) => self.tls_accept_plain(),
                Some(false) => self.tls.plain_prompt = None,
                None => {}
            }
        }
        if let Some(m) = self.tls.mismatch.clone() {
            if m.confirming {
                let answer = ui_kit::confirm(ctx, "tls_trust", "Confiar no novo código?", TRUST_NEW, Kind::Danger, true, |ui| {
                    ui_kit::body_text(ui, "Só confirme se quem hospeda te disse que o servidor foi reinstalado e o código novo bate com o da janela do servidor.");
                    ui.add_space(space::S);
                    ui.label(RichText::new(format!("Código salvo: {}", vox_proto::tls_server_code(&m.expected))).monospace().color(theme::NEVOA));
                    ui.label(RichText::new(format!("Código novo:  {}", vox_proto::tls_server_code(&m.got))).monospace().color(theme::GIZ));
                });
                match answer {
                    Some(true) => self.tls_trust_new(),
                    Some(false) => self.tls.mismatch = None,
                    None => {}
                }
            } else {
                self.mismatch_window(ctx, &m);
            }
        }
        if self.tls.security_open {
            self.security_window(ctx);
        }
    }

    fn mismatch_window(&mut self, ctx: &egui::Context, m: &Mismatch) {
        let mut pick = ctx.input(|i| i.key_pressed(egui::Key::Escape)).then_some(0);
        egui::Window::new(MISMATCH_TITLE)
            .id(egui::Id::new("tls_mismatch"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(ui_kit::dialog_frame(&ctx.global_style()))
            .show(ctx, |ui| {
                ui.set_width(ui_kit::dialog_w(ctx, ui_kit::DIALOG_W + 80.0));
                ui.spacing_mut().item_spacing.y = 6.0;
                ui.horizontal(|ui| {
                    ui.label(RichText::new(ph::WARNING).font(theme::body(20.0)).color(theme::AO_VIVO));
                    ui_kit::dialog_title(ui, MISMATCH_TITLE);
                });
                ui.add_space(space::S);
                ui_kit::dialog_scroll(ui, "tls_mismatch_body", ui_kit::DIALOG_RESERVE, |ui| {
                    ui_kit::body_text(ui, MISMATCH_TEXT);
                    ui_kit::hint(ui, format!("Por segurança, nada foi enviado (nem senha, nem login). Endereço: {}", m.url));
                });
                ui.add_space(space::XL);
                ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 32.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui_kit::button(ui, Kind::Danger, None, TRUST_NEW).clicked() {
                        pick = Some(2);
                    }
                    if ui_kit::button(ui, Kind::Secondary, None, NEW_INVITE).clicked() {
                        pick = Some(1);
                    }
                    if ui_kit::button(ui, Kind::Secondary, None, "Cancelar").clicked() {
                        pick = Some(0);
                    }
                });
            });
        match pick {
            Some(0) => self.tls.mismatch = None,
            Some(1) => self.tls_use_new_invite(),
            Some(2) => {
                if let Some(m) = self.tls.mismatch.as_mut() {
                    m.confirming = true;
                }
            }
            _ => {}
        }
    }

    fn security_window(&mut self, ctx: &egui::Context) {
        let mut open = !ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let lock = self.tls_lock();
        let session = self.tls.session;
        egui::Window::new(SECURITY_TITLE)
            .id(egui::Id::new("tls_security"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(ui_kit::dialog_frame(&ctx.global_style()))
            .show(ctx, |ui| {
                ui.set_width(ui_kit::dialog_w(ctx, ui_kit::DIALOG_W + 40.0));
                ui.spacing_mut().item_spacing.y = 6.0;
                ui_kit::dialog_title(ui, SECURITY_TITLE);
                ui.add_space(space::S);
                ui_kit::dialog_scroll(ui, "tls_security_body", ui_kit::DIALOG_RESERVE, |ui| {
                    if let Some((icon, text, color)) = lock {
                        ui.label(RichText::new(format!("{icon}  {text}")).font(theme::body(14.0)).color(if session.is_some() { theme::ONLINE } else { color }));
                    }
                    match session {
                        Some(pin) => {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(code_line(&pin)).monospace().color(theme::GIZ));
                                if ui_kit::button_sized(ui, Kind::Ghost, Size::Small, Some(ph::COPY), "Copiar").clicked() {
                                    ui.ctx().copy_text(vox_proto::tls_server_code(&pin));
                                }
                            });
                            ui_kit::hint(
                                ui,
                                "Senhas, mensagens e a voz viajam criptografadas até o servidor. Para conferir que ninguém está no meio, compare este \
                                 código com o que aparece na janela do servidor (ou no painel \"Meu servidor\" de quem hospeda): têm de ser iguais.",
                            );
                        }
                        None => {
                            ui_kit::hint(
                                ui,
                                "Este servidor é de uma versão antiga do concord: senhas e mensagens passam pela rede sem criptografia. Peça para \
                                 quem hospeda atualizar o servidor para o concord 1.1 ou mais novo.",
                            );
                        }
                    }
                });
                ui.add_space(space::L);
                ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 32.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui_kit::button(ui, Kind::Secondary, None, "Fechar").clicked() {
                        open = false;
                    }
                });
            });
        if !open {
            self.tls.security_open = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_texts_are_the_plans() {
        assert_eq!(LOCK_ON, "Conexão protegida");
        assert_eq!(LOCK_OFF, "Conexão sem proteção (servidor antigo)");
        assert_eq!(MISMATCH_TITLE, "A identidade do servidor mudou");
        assert_eq!(MISMATCH_TEXT, "Se quem hospeda reinstalou o servidor, peça um convite novo. Se não, alguém pode estar interceptando a conexão.");
        assert_eq!(OLD_APPS, "Aceitar apps antigos sem conexão protegida");
        assert_eq!((NEW_INVITE, TRUST_NEW, PLAIN_YES), ("Usar um convite novo", "Confiar no novo código", "Conectar sem proteção"));
        let line = code_line(&[0u8; 32]);
        assert_eq!(line, "Código do servidor: 0000-0000-0000-0000");
    }
}
