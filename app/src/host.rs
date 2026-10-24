//! "Criar / abrir servidor" (v0.8): host mode in the app, and invites.
//!
//! - The home screen has two paths: "Conectar" (the saved server list of `servers.rs`, and an
//!   address or a `vox1-` invite code) and "Criar / abrir servidor" (this module).
//! - Creating a server makes a folder under `%LOCALAPPDATA%\concord\host` (`VOX_HOST_DIR` in tests);
//!   opening one makes sure the voice server is installed (downloaded once, checked against the
//!   pinned SHA-256), then runs the hub in this process (`voxhub_server::host::HostRunner`) and
//!   logs into it through 127.0.0.1, which makes the host its Dono.
//! - The server lives while the app is open or in the tray; leaving the app asks first ("Seu
//!   servidor vai desligar e todo mundo cai").
//! - "Meu servidor": status, ports, the router (UPnP), the public address and CGNAT, who is
//!   online, the server password, "Parar servidor", and the invite tools. The same invite tools
//!   are under "Configurações do servidor" → "Convites" on any hub where my role allows it
//!   (Admin and Dono), the classic `concord-server.exe` included.
//! - v1.0: the panel and the home tab in the kit's groups and rows; "Parar servidor", leaving the
//!   app while hosting and "Revogar" ask in the kit's dialog (`host_windows`).

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::Instant;

use eframe::egui::{self, Align, Align2, Layout, RichText, Sense, Vec2};
use vox_client_core::hub::HubClient;
use vox_proto::{invite_token_from_hex, Invite, InviteAddr, InviteCode, InviteCodeError, InviteCreateRequest, InviteCreated, InviteId, InviteState};
use voxhub_server::host::nat::{CgnatSign, Reach};
use voxhub_server::host::{self as hosting, livekit_fetch, HostRunner, HostStatus, Hosted, Phase, RunOptions, UpnpState};

use crate::app::{App, Screen, GREEN, RED};
use crate::backend::Cmd;
use crate::theme::{self, ph, size, space};
use crate::ui_kit::{self, IconState, Kind, Size};

/// The home screen's two paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HomeTab {
    #[default]
    Connect,
    Host,
}

/// Joining with an invite code: the token and the hub URLs to try, in order. v0.9.1: a saved
/// server's addresses too (no token: an invite is used up by the first login).
#[derive(Debug, Clone)]
pub struct InviteJoin {
    pub token: Option<String>,
    pub urls: Vec<String>,
}

// ---------------------------------------------------------------- backend side

/// The invite calls, run by the backend (`Cmd::Invite`); the answer comes back as `Ev::Invite`.
#[derive(Debug, Clone)]
pub enum InviteCmd {
    List,
    Create(InviteCreateRequest),
    Revoke(InviteId),
}

#[derive(Debug)]
pub enum InviteReply {
    List(Vec<Invite>),
    Created(InviteCreated),
    Revoked,
}

#[derive(Debug)]
pub struct InviteEv {
    pub cmd: InviteCmd,
    pub result: Result<InviteReply, String>,
}

pub async fn run_invite(hub: &HubClient, cmd: &InviteCmd) -> Result<InviteReply, String> {
    let r = match cmd {
        InviteCmd::List => hub.invites().await.map(InviteReply::List),
        InviteCmd::Create(req) => hub.create_invite(req).await.map(InviteReply::Created),
        InviteCmd::Revoke(id) => hub.revoke_invite(*id).await.map(|_| InviteReply::Revoked),
    };
    r.map_err(|e| {
        tracing::warn!(error = %format!("{e:#}"), ?cmd, "invite call failed");
        let l = format!("{e:#}").to_lowercase();
        if l.contains("too many active invites") {
            format!("Já existem {} convites valendo. Revogue alguns antes de criar outro.", vox_proto::INVITE_MAX_ACTIVE)
        } else if l.contains("http 404") || l.contains("http 405") {
            "Este servidor é de uma versão antiga, sem convites. Peça para quem hospeda atualizar o concord.".into()
        } else {
            crate::roles::admin_error(&e)
        }
    })
}

/// What a code that does not decode means, for people.
pub fn code_error_text(e: InviteCodeError) -> &'static str {
    match e {
        InviteCodeError::NotAnInvite | InviteCodeError::Typo => {
            "O código de convite está incompleto ou tem um erro de digitação. Copie e cole de novo, inteiro (começa com vox1-)."
        }
        InviteCodeError::NewerVersion => "Este convite foi feito por uma versão mais nova do concord. Atualize o app e tente de novo.",
        InviteCodeError::Damaged => "Este código de convite está danificado. Peça um novo a quem te convidou.",
    }
}

/// This computer's interface addresses (to see whether an invite's LAN address is on our network).
pub(crate) fn local_addrs() -> Vec<(IpAddr, u8)> {
    let mut v = voxhub_server::linkpreview::ssrf::interface_addresses();
    v.push((IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 8));
    v
}

// ---------------------------------------------------------------- UI state

/// Validity choices ("Validade").
pub const EXPIRY: [(Option<u64>, &str); 4] = [(Some(3600), "1 hora"), (Some(24 * 3600), "24 horas"), (Some(7 * 24 * 3600), "7 dias"), (None, "Nunca expira")];
/// Use limits ("Pode ser usado por").
pub const USES: [(Option<u32>, &str); 4] = [(Some(1), "1 pessoa"), (Some(5), "5 pessoas"), (Some(25), "25 pessoas"), (None, "Sem limite")];

pub struct InviteUi {
    pub list: Vec<Invite>,
    pub loaded: bool,
    pub busy: bool,
    pub error: Option<String>,
    pub expiry: usize,
    pub uses: usize,
    /// Codes made in this session (the hub keeps only a hash of each token).
    pub codes: HashMap<InviteId, String>,
    /// The code just made, shown big.
    pub last: Option<(InviteId, String)>,
    pub confirm_revoke: Option<InviteId>,
    copied_at: Option<Instant>,
}

impl Default for InviteUi {
    fn default() -> Self {
        InviteUi { list: Vec::new(), loaded: false, busy: false, error: None, expiry: 1, uses: 1, codes: HashMap::new(), last: None, confirm_revoke: None, copied_at: None }
    }
}

enum Prepared {
    Progress(u64, Option<u64>),
    Done(Result<Option<PathBuf>, String>),
}

pub struct HostUi {
    pub root: PathBuf,
    pub tab: HomeTab,
    pub servers: Vec<Hosted>,
    pub(crate) listed: bool,
    pub new_name: String,
    pub error: Option<String>,
    pub runner: Option<HostRunner>,
    pub hosted: Option<Hosted>,
    /// Voice-server download progress (bytes, total) while it runs.
    pub fetching: Option<(u64, Option<u64>)>,
    /// Getting the voice server ready for this server (checking / downloading).
    preparing: Option<(Hosted, Receiver<Prepared>)>,
    /// Log into our own server once it runs (once per start).
    pub(crate) auto_login: bool,
    pub panel_open: bool,
    pub confirm_stop: bool,
    pub confirm_quit: bool,
    quit_confirmed: bool,
    pub show_password: bool,
    pub inv: InviteUi,
    /// The server (and me) the invite list belongs to.
    inv_for: String,
}

impl HostUi {
    pub fn new() -> Self {
        HostUi {
            root: hosting::root_dir(),
            tab: HomeTab::Connect,
            servers: Vec::new(),
            listed: false,
            new_name: String::new(),
            error: None,
            runner: None,
            hosted: None,
            fetching: None,
            preparing: None,
            auto_login: false,
            panel_open: false,
            confirm_stop: false,
            confirm_quit: false,
            quit_confirmed: false,
            show_password: false,
            inv: InviteUi::default(),
            inv_for: String::new(),
        }
    }

    /// A server of mine is starting or running.
    pub fn hosting(&self) -> bool {
        self.preparing.is_some() || self.runner.as_ref().is_some_and(|r| r.is_live())
    }

    pub fn status(&self) -> Option<HostStatus> {
        self.runner.as_ref().map(|r| r.status())
    }
}

// ---------------------------------------------------------------- texts

fn phase_text(p: &Phase) -> (String, egui::Color32) {
    match p {
        Phase::Starting => ("Ligando...".into(), theme::AVISO),
        Phase::Network => ("Abrindo as portas no roteador...".into(), theme::AVISO),
        Phase::Running => ("Ligado".into(), GREEN),
        Phase::Stopping => ("Desligando...".into(), theme::AVISO),
        Phase::Stopped => ("Desligado".into(), RED),
        Phase::Failed(e) => (format!("Não ligou: {e}"), RED),
    }
}

/// The reachability part of the panel, in plain Portuguese: (text, colour).
pub fn reach_text(st: &HostStatus) -> (String, egui::Color32) {
    let ports = st.ports.map(|p| p.describe()).unwrap_or_default();
    let lan = st.lan.map(|(ip, _)| ip.to_string()).unwrap_or_else(|| "este computador".into());
    let manual = format!(
        "Para amigos de fora da sua casa entrarem, abra (\"encaminhe\") estas portas no roteador para {lan}: {ports}. \
         Procure por \"redirecionamento de portas\" ou \"port forwarding\" na página do roteador."
    );
    match (st.reach, &st.upnp) {
        (Some(Reach::Cgnat(sign)), _) => {
            let why = match sign {
                CgnatSign::SharedRange => "o roteador recebeu um endereço de CGNAT (100.64.x.x) da operadora",
                CgnatSign::PrivateWan => "há outro roteador (o modem da operadora) na frente do seu",
                CgnatSign::Mismatch => "o endereço do roteador não é o que a internet vê",
            };
            (
                format!(
                    "Sua internet não deixa receber conexões de fora ({why}). Isso é comum no Brasil (CGNAT) e não dá para resolver \
                     neste computador: quem está na mesma rede da sua casa entra normalmente, mas amigos pela internet não conseguem \
                     entrar direto. Saídas: pedir à operadora um \"IP público\" (às vezes é grátis), ou todos instalarem o Tailscale \
                     ou o ZeroTier, que criam uma rede particular entre vocês. Um retransmissor do concord é uma ideia para o futuro."
                ),
                RED,
            )
        }
        (Some(Reach::Direct), _) => ("Este computador tem endereço público: amigos pela internet conseguem entrar.".into(), GREEN),
        (_, UpnpState::Mapped(m)) if m.complete() => (
            format!("O roteador abriu as portas sozinho ({ports}). Amigos pela internet conseguem entrar com o convite."),
            GREEN,
        ),
        (_, UpnpState::Mapped(m)) => {
            let bad: Vec<String> = m.failed.iter().map(|(p, n, why)| format!("{n} {} ({why})", p.name())).collect();
            (format!("O roteador abriu só parte das portas; faltaram: {}. {manual}", bad.join(", ")), theme::AVISO)
        }
        (_, UpnpState::Unavailable(_)) => (format!("O roteador não aceitou abrir as portas sozinho (UPnP desligado ou sem suporte). {manual}"), theme::AVISO),
        (_, UpnpState::Searching) => ("Procurando o roteador...".into(), theme::AVISO),
        (_, UpnpState::Off) => ("Portas no roteador: não mexidas (modo de teste).".into(), theme::AVISO),
    }
}

const FIREWALL_TEXT: &str = "Se o Windows perguntar se o concord (e o \"livekit-server\", o servidor de voz) pode usar a rede, clique em \"Permitir acesso\". \
     Sem isso, só este computador consegue entrar.";

const HONEST_TEXT: &str = "O código esconde o endereço do servidor de quem só olha, mas não é um segredo absoluto: o concord é aberto, \
     e quem entra no servidor fica sabendo o IP (é assim que a conexão funciona). A proteção de verdade é o convite: ele vence, \
     tem limite de usos e pode ser revogado.";

fn when_text(ms: i64) -> String {
    crate::app::format_time(ms, crate::app::now_ms(), crate::app::local_offset_min())
}

/// `http://203.0.113.5:7890/x` -> (`203.0.113.5`, 7890); `[2001:db8::1]` keeps no brackets.
pub(crate) fn url_host_port(url: &str) -> Option<(String, Option<u16>)> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split('/').next()?;
    if let Some(r) = authority.strip_prefix('[') {
        let (h, after) = r.split_once(']')?;
        return Some((h.to_string(), after.strip_prefix(':').and_then(|p| p.parse().ok())));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) => Some((h.to_string(), p.parse().ok())),
        None => Some((authority.to_string(), None)),
    }
}

/// One address for an invite from a host name / IP text (loopback and nonsense are skipped).
fn push_addr(addrs: &mut Vec<InviteAddr>, host: &str, lan_prefix: u8) {
    let host = host.trim();
    let a = match host.parse::<IpAddr>() {
        Ok(ip) if ip.is_loopback() || ip.is_unspecified() => return,
        Ok(IpAddr::V4(v4)) if v4.is_private() => InviteAddr::Lan(v4, lan_prefix),
        Ok(ip) => InviteAddr::Public(ip),
        Err(_) if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") && host.len() <= 253 && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')) => {
            InviteAddr::Host(host.to_ascii_lowercase())
        }
        Err(_) => return,
    };
    if !addrs.contains(&a) {
        addrs.push(a);
    }
}

// ---------------------------------------------------------------- the app side

impl App {
    /// The server I host, while I am logged into it (its own panel builds invites from what the
    /// router and STUN found instead of from the hub's hints).
    fn own_server(&self) -> Option<HostStatus> {
        let st = self.host.status().filter(|s| s.phase == Phase::Running)?;
        let port = st.hub_addr?.port();
        let (h, p) = url_host_port(&self.session_url)?;
        let loopback = h.eq_ignore_ascii_case("localhost") || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
        (self.me.is_some() && loopback && p == Some(port)).then_some(st)
    }

    /// The `vox1-` code of a new invite.
    fn invite_code(&self, c: &InviteCreated) -> Result<String, String> {
        let token = invite_token_from_hex(&c.token).ok_or("O servidor mandou um convite inválido.")?;
        let (hub_port, addrs) = match self.own_server() {
            Some(st) => (st.hub_addr.map_or(c.hints.hub_port, |a| a.port()), st.invite_addrs()),
            None => {
                let mut addrs = Vec::new();
                let prefix = if c.hints.lan_prefix == 0 { 24 } else { c.hints.lan_prefix };
                let used = url_host_port(&self.session_url);
                if let Some((h, _)) = &used {
                    push_addr(&mut addrs, h, prefix);
                }
                push_addr(&mut addrs, &c.hints.public_host, prefix);
                if let Some(ip) = c.hints.lan_ipv4 {
                    push_addr(&mut addrs, &ip.to_string(), prefix);
                }
                let port = used.and_then(|(_, p)| p).unwrap_or(c.hints.hub_port);
                (port, addrs)
            }
        };
        if addrs.is_empty() {
            return Err("Não deu para montar o convite: o servidor não sabe o próprio endereço público. Quem hospeda pode definir \
                        public_host no config.toml."
                .into());
        }
        // v1.1 (T0): the certificate goes into the code (layout 2): my own server's; else the one
        // this session is pinned to (checked by this app); else, on the server's own PC
        // (loopback), the hub's word for it. Without one the code stays the layout every app reads.
        let tls_pin = match self.own_server() {
            Some(st) => st.tls_pin,
            None => self.tls.session.or_else(|| {
                let local = url_host_port(&self.session_url).is_some_and(|(h, _)| h.eq_ignore_ascii_case("localhost") || h.trim_matches(|c| c == '[' || c == ']').parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()));
                c.hints.tls_pin.as_deref().filter(|_| local).and_then(vox_proto::tls_pin_from_hex)
            }),
        };
        Ok(InviteCode { token, hub_port, addrs, tls_pin }.encode())
    }

    pub fn invite_cmd(&mut self, cmd: InviteCmd) {
        self.host.inv.busy = true;
        self.backend.send(Cmd::Invite(cmd));
    }

    /// "Gerar convite" with the chosen validity and use limit.
    pub fn create_invite(&mut self) {
        let req = InviteCreateRequest { expires_secs: EXPIRY[self.host.inv.expiry.min(3)].0, max_uses: USES[self.host.inv.uses.min(3)].0 };
        self.host.inv.error = None;
        self.invite_cmd(InviteCmd::Create(req));
    }

    pub fn on_invite_event(&mut self, ev: InviteEv) {
        let inv = &mut self.host.inv;
        inv.busy = false;
        let reply = match ev.result {
            Ok(r) => r,
            Err(e) => {
                inv.error = Some(e);
                return;
            }
        };
        match reply {
            InviteReply::List(list) => {
                inv.list = list;
                inv.loaded = true;
            }
            InviteReply::Created(c) => match self.invite_code(&c) {
                Ok(code) => {
                    tracing::info!(id = c.invite.id, len = code.len(), "invite created");
                    let inv = &mut self.host.inv;
                    inv.codes.insert(c.invite.id, code.clone());
                    inv.last = Some((c.invite.id, code));
                    inv.list.retain(|i| i.id != c.invite.id);
                    inv.list.insert(0, c.invite);
                }
                Err(e) => self.host.inv.error = Some(e),
            },
            InviteReply::Revoked => {
                inv.confirm_revoke = None;
                if let InviteCmd::Revoke(id) = ev.cmd {
                    if let Some(i) = inv.list.iter_mut().find(|i| i.id == id) {
                        i.revoked = true;
                    }
                    if inv.last.as_ref().is_some_and(|(l, _)| *l == id) {
                        inv.last = None;
                    }
                }
            }
        }
    }

    /// Joining with a `vox1-` code typed in "Servidor ou convite".
    pub(crate) fn connect_invite(&mut self, text: &str, user: &str) {
        let code = match InviteCode::decode(text) {
            Ok(c) => c,
            Err(e) => {
                tracing::info!(error = %e, "invite code refused");
                self.form.error = Some(code_error_text(e).into());
                return;
            }
        };
        let urls = code.candidate_urls(&local_addrs());
        let Some(first) = urls.first().cloned() else {
            self.form.error = Some(code_error_text(InviteCodeError::Damaged).into());
            return;
        };
        self.form.busy = true;
        self.form.error = None;
        let personal = Some(self.form.personal.clone()).filter(|p| !p.is_empty());
        // A token this computer already has for one of the invite's addresses. v1.0.1: it goes
        // only to the server that issued it (its identity, else exactly that address), never to
        // another address of the code: anybody can write an invite code with any addresses.
        let token_at = urls.iter().map(|u| crate::backend::normalize_url(u)).find_map(|u| self.settings.device_token(&u, user).map(|t| (u, t)));
        let guard = crate::backend::Guard {
            token: token_at.as_ref().map(|(at, _)| crate::backend::Binding { home: at.clone(), server_id: self.settings.device_token_server(at, user), tls_pin: self.settings.device_token_pin(at, user) }),
            password: None,
            confirmed: None,
        };
        // v1.1 (T0): a code with a pin means pinned TLS only (rule 1); without one (an older
        // hub's), TLS when the hub has it, else plain HTTP (rule 3).
        let tls = crate::backend::TlsPlan { pin: self.tls.trust_override.take().or(code.tls_pin), source: crate::backend::PinSource::Invite, plain_ok: false };
        let device_token = token_at.map(|(_, t)| t);
        tracing::info!(candidates = urls.len(), user, personal = personal.is_some(), device_token = device_token.is_some(), tls_pin = code.tls_pin.is_some(), "connecting with an invite");
        // v0.9.1: the saved entry keeps the invite's addresses (never its token).
        self.servers_login_started(device_token.is_some(), Some((code.hub_port, &code.addrs)));
        self.backend.send(Cmd::Connect {
            url: first,
            user: user.to_string(),
            password: String::new(),
            personal,
            device_token,
            // The device token is what lets this computer back in later: always ask for it.
            remember_device: true,
            invite: Some(InviteJoin { token: Some(code.token_hex()), urls }),
            guard,
            tls,
        });
    }

    // ------------------------------------------------------------ hosting

    pub(crate) fn refresh_servers(&mut self) {
        self.host.servers = hosting::list(&self.host.root);
        self.host.listed = true;
    }

    /// "Criar servidor": a new server, opened at once.
    pub fn host_create(&mut self, name: &str) {
        self.host.error = None;
        if self.form.user.trim().chars().count() < 2 {
            self.host.error = Some("Informe seu nome (pelo menos 2 letras) antes de criar o servidor.".into());
            return;
        }
        match hosting::create(&self.host.root, name) {
            Ok(h) => {
                self.host.new_name.clear();
                self.refresh_servers();
                self.host_open(h);
            }
            Err(e) => self.host.error = Some(format!("{e:#}")),
        }
    }

    /// "Abrir servidor": make sure the voice server is there (download once), then run it.
    pub fn host_open(&mut self, h: Hosted) {
        if self.host.hosting() {
            self.host.error = Some("Já tem um servidor seu ligado. Pare ele antes de abrir outro.".into());
            return;
        }
        if self.form.user.trim().chars().count() < 2 {
            self.host.error = Some("Informe seu nome (pelo menos 2 letras) antes de abrir o servidor.".into());
            return;
        }
        self.host.error = None;
        let (tx, rx) = channel();
        let bin_dir = hosting::bin_dir(&self.host.root);
        let wake = self.repaint.clone();
        tracing::info!(id = h.id, name = h.name, "host: opening");
        let spawned = std::thread::Builder::new().name("voice-server".into()).spawn(move || {
            let r = prepare_voice_server(&bin_dir, &|n, total| {
                let _ = tx.send(Prepared::Progress(n, total));
                wake.wake();
            });
            let _ = tx.send(Prepared::Done(r));
            wake.wake();
        });
        if spawned.is_err() {
            self.host.error = Some("Não foi possível preparar o servidor.".into());
            return;
        }
        self.host.preparing = Some((h, rx));
    }

    fn host_start(&mut self, h: Hosted, livekit: Option<PathBuf>) {
        let r = self.repaint.clone();
        let runner = HostRunner::start(&h, RunOptions::from_env(livekit), Arc::new(move || r.wake()));
        self.host.runner = Some(runner);
        self.host.hosted = Some(h);
        self.host.auto_login = true;
    }

    /// "Parar servidor" (also logs out of it first).
    pub fn host_stop(&mut self) {
        if self.own_server().is_some() || (self.me.is_some() && self.host.runner.is_some() && self.session_url.contains("127.0.0.1")) {
            self.logout();
        }
        if let Some(r) = &self.host.runner {
            r.stop();
        }
        self.host.confirm_stop = false;
        self.host.auto_login = false;
    }

    /// Every frame: the voice server download, the server's state, the automatic login into it,
    /// and "leaving the app stops the server" (before `notify_tick` turns a close into the tray).
    pub(crate) fn host_tick(&mut self, ctx: &egui::Context) {
        if let Some((_, rx)) = &self.host.preparing {
            let mut done = None;
            while let Ok(m) = rx.try_recv() {
                match m {
                    Prepared::Progress(n, t) => self.host.fetching = Some((n, t)),
                    Prepared::Done(r) => done = Some(r),
                }
            }
            if let Some(r) = done {
                let (h, _) = self.host.preparing.take().expect("preparing");
                self.host.fetching = None;
                match r {
                    Ok(bin) => self.host_start(h, bin),
                    Err(e) => {
                        tracing::error!(error = %e, "host: voice server not ready");
                        self.host.error = Some(e);
                    }
                }
            }
        }

        if let Some(st) = self.host.status() {
            match &st.phase {
                Phase::Running if self.host.auto_login && self.screen == Screen::Connect && !self.form.busy => {
                    self.host.auto_login = false;
                    if let (Some(addr), Some(h)) = (st.hub_addr, self.host.hosted.clone()) {
                        tracing::info!(port = addr.port(), "host: logging into my own server");
                        self.form.url = format!("http://127.0.0.1:{}", addr.port());
                        self.form.password = h.password.clone();
                        self.connect();
                    }
                }
                Phase::Failed(e) => {
                    self.host.error = Some(format!("O servidor não ligou: {e}"));
                    self.host.runner = None;
                    self.host.hosted = None;
                    self.host.panel_open = false;
                }
                Phase::Stopped => {
                    tracing::info!("host: server stopped");
                    self.host.runner = None;
                    self.host.hosted = None;
                    self.host.panel_open = false;
                }
                _ => {}
            }
        }

        // Logging into my own server needs my personal password (no remembered device for it yet):
        // the field for it is on the "Conectar" side.
        if self.form.need_personal && self.host.tab == HomeTab::Host && self.screen == Screen::Connect {
            self.host.tab = HomeTab::Connect;
        }

        // The invite list is per server (and per login).
        let key = format!("{}|{:?}", self.session_url, self.me.as_ref().map(|u| u.id));
        if key != self.host.inv_for {
            self.host.inv_for = key;
            self.host.inv = InviteUi::default();
        }

        // Joined with a code: from now on the field shows the address that worked.
        if self.screen == Screen::Main && vox_proto::looks_like_invite(&self.form.url) {
            self.form.url = self.session_url.clone();
        }

        // Leaving the app (tray "Sair", or the close button when it does not go to the tray).
        let closing = ctx.input(|i| i.viewport().close_requested());
        if closing && self.host.hosting() && !self.host.quit_confirmed && self.auto.is_none() && !self.notify.hides_on_close(self.settings.notify.tray_on_close) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.notify.hidden {
                self.show_window(ctx);
            }
            self.host.confirm_quit = true;
        }
    }

    /// The app is closing: stop the server (and remove the router's port mappings).
    pub(crate) fn host_on_exit(&mut self) {
        if let Some(mut r) = self.host.runner.take() {
            tracing::info!("host: app closing, stopping my server");
            r.stop_and_wait(std::time::Duration::from_secs(8));
        }
    }

    // ------------------------------------------------------------ UI

    /// "Criar ou abrir servidor" on the home screen: my running server (with "Entrar nele", its
    /// panel and "Parar servidor"), or my name, a new server and the ones made before.
    pub(crate) fn host_home(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.y = 6.0;
        if let Some(st) = self.host.status() {
            let (text, color) = phase_text(&st.phase);
            ui_kit::group_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = space::M;
                    ui.label(RichText::new(ph::HARD_DRIVES).font(theme::body(22.0)).color(theme::NEVOA));
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.add(egui::Label::new(RichText::new(&st.name).font(theme::display(size::TITLE)).color(theme::GIZ)).truncate().selectable(false));
                        status_line(ui, &text, color);
                    });
                });
                ui.add_space(space::L);
                ui.horizontal_wrapped(|ui| {
                    if st.phase == Phase::Running && ui_kit::button(ui, Kind::Primary, Some(ph::ARROW_RIGHT), "Entrar nele").clicked() {
                        self.host.auto_login = true;
                    }
                    if ui_kit::button(ui, Kind::Secondary, Some(ph::WRENCH), "Painel do servidor").clicked() {
                        self.host.panel_open = true;
                    }
                    if ui_kit::button(ui, Kind::Ghost, Some(ph::STOP), "Parar servidor").clicked() {
                        self.host.confirm_stop = true;
                    }
                });
            });
            if let Some(e) = &self.form.error {
                ui.add_space(space::M);
                notice(ui, ui_kit::Notice::Error, e);
            }
            return;
        }
        // macOS: the voice server (LiveKit) has no official macOS build to download yet (M4).
        if crate::platform::HOSTING_SOON {
            notice(ui, ui_kit::Notice::Info, &format!("Hospedar um servidor pelo app: {}. Por enquanto, entre no servidor de um amigo com o endereço ou um convite.", crate::platform::SOON_MAC_LC));
            return;
        }
        let busy = self.host.preparing.is_some();
        ui_kit::field_label(ui, "Seu nome");
        ui.add(ui_kit::field(&mut self.form.user).hint_text("como os outros vão ver você").desired_width(ui.available_width().min(420.0)));
        ui.add_space(space::L);
        ui_kit::section(ui, "Criar um servidor novo");
        let mut create = false;
        ui.horizontal(|ui| {
            let r = ui.add(ui_kit::field(&mut self.host.new_name).hint_text("nome do servidor").desired_width((ui.available_width() - 150.0).clamp(160.0, 270.0)));
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let can = !busy && !self.host.new_name.trim().is_empty();
            create = (ui.add_enabled_ui(can, |ui| ui_kit::button(ui, Kind::Primary, Some(ph::PLUS), "Criar servidor")).inner.clicked() || enter) && can;
        });
        if create {
            let name = self.host.new_name.clone();
            self.host_create(&name);
        }
        if !self.host.servers.is_empty() {
            ui.add_space(space::L);
            ui_kit::section(ui, "Seus servidores");
            let mut open = None;
            ui_kit::group_frame().inner_margin(egui::Margin::symmetric(6, 6)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 0.0;
                for h in &self.host.servers {
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 40.0), Sense::hover());
                    let mut row = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(Vec2::new(8.0, 0.0))).layout(Layout::left_to_right(Align::Center)));
                    row.spacing_mut().item_spacing.x = space::M;
                    row.label(RichText::new(ph::HARD_DRIVES).font(theme::body(size::ICON)).color(theme::NEVOA));
                    row.add(egui::Label::new(RichText::new(&h.name).font(theme::semibold(13.5)).color(theme::GIZ)).truncate().selectable(false));
                    row.label(RichText::new(format!("porta {}", h.ports.hub)).font(theme::body(12.0)).color(theme::NEVOA));
                    row.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add_enabled_ui(!busy, |ui| ui_kit::button_sized(ui, Kind::Secondary, Size::Small, None, "Abrir")).inner.clicked() {
                            open = Some(h.clone());
                        }
                    });
                }
            });
            if let Some(h) = open {
                self.host_open(h);
            }
        }
        if busy {
            ui.add_space(space::L);
            match self.host.fetching {
                Some((n, total)) => {
                    ui_kit::body_text(ui, "Baixando o servidor de voz (só na primeira vez)...");
                    let frac = total.map_or(0.0, |t| n as f32 / t.max(1) as f32);
                    ui.add(egui::ProgressBar::new(frac).show_percentage().desired_width(ui.available_width().min(420.0)));
                    ui_kit::hint(ui, format!("{:.1} MB", n as f64 / 1_048_576.0));
                }
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui_kit::body_text(ui, "Preparando o servidor...");
                    });
                }
            }
        }
        if let Some(e) = &self.host.error {
            ui.add_space(space::M);
            notice(ui, ui_kit::Notice::Error, e);
        }
        ui.add_space(space::XL);
        ui_kit::hint(
            ui,
            "O servidor roda neste computador e fica ligado enquanto o concord estiver aberto (ou na bandeja). \
             Na primeira vez o app baixa o servidor de voz (uns 20 MB). Depois é só mandar um convite para os amigos.",
        );
    }

    /// The host panel and the hosting confirmations ("Parar servidor", leaving the app while
    /// hosting, "Revogar convite"): the kit's dialog, the action on the right.
    pub(crate) fn host_windows(&mut self, ctx: &egui::Context) {
        self.host_panel(ctx);
        let name = self.host.hosted.as_ref().map(|h| h.name.clone()).unwrap_or_default();
        if self.host.confirm_quit {
            let answer = ui_kit::confirm(ctx, "host_quit", "Sair do concord?", "Sair e desligar", Kind::Danger, true, |ui| {
                ui_kit::body_text(ui, format!("Seu servidor \"{name}\" vai desligar e todo mundo cai."));
                ui_kit::hint(ui, "Para deixar o servidor ligado, feche a janela pelo X: o concord fica na bandeja.");
            });
            match answer {
                Some(true) => {
                    tracing::info!("host: leaving the app, the server goes down");
                    self.host.confirm_quit = false;
                    self.host.quit_confirmed = true;
                    if let Some(r) = &self.host.runner {
                        r.stop();
                    }
                    self.notify.allow_quit();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Some(false) => {
                    self.host.confirm_quit = false;
                    self.notify.cancel_quit();
                }
                None => {}
            }
        } else if self.host.confirm_stop {
            if self.host.status().is_none() {
                self.host.confirm_stop = false;
                return;
            }
            let answer = ui_kit::confirm(ctx, "host_stop", "Parar servidor", "Parar servidor", Kind::Danger, true, |ui| {
                ui_kit::body_text(ui, format!("Parar o servidor \"{name}\"?"));
                ui_kit::hint(ui, "Todo mundo cai, você também. Dá para abrir de novo depois em \"Criar ou abrir servidor\".");
            });
            match answer {
                Some(true) => self.host_stop(),
                Some(false) => self.host.confirm_stop = false,
                None => {}
            }
        } else if let Some(id) = self.host.inv.confirm_revoke {
            let answer = ui_kit::confirm(ctx, "invite_revoke", "Revogar convite", "Revogar", Kind::Danger, !self.host.inv.busy, |ui| {
                ui_kit::body_text(ui, format!("Revogar o convite #{id}?"));
                ui_kit::hint(ui, "Ninguém mais entra com este código. Quem já entrou com ele continua no servidor.");
            });
            match answer {
                Some(true) => {
                    self.host.inv.error = None;
                    self.invite_cmd(InviteCmd::Revoke(id));
                }
                Some(false) => self.host.inv.confirm_revoke = None,
                None => {}
            }
        }
    }

    /// Any of the hosting confirmations is up (Esc belongs to it).
    pub(crate) fn host_confirming(&self) -> bool {
        self.host.confirm_quit || self.host.confirm_stop || self.host.inv.confirm_revoke.is_some()
    }

    /// "Meu servidor": the status and the addresses, what the router and the internet allow (UPnP,
    /// CGNAT), the invites and "Parar servidor", in the kit's groups.
    fn host_panel(&mut self, ctx: &egui::Context) {
        if !self.host.panel_open {
            return;
        }
        let Some(st) = self.host.status() else {
            self.host.panel_open = false;
            return;
        };
        let mine = self.own_server().is_some();
        if mine && !self.host.inv.loaded && !self.host.inv.busy && self.my_role().can_admin() {
            self.invite_cmd(InviteCmd::List);
        }
        let mut open = !(ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !self.host_confirming() && !egui::Popup::is_any_open(ctx));
        let screen = ctx.content_rect();
        // (I6: inside a small or zoomed-in window too: narrower, shorter, the body scrolls.)
        let panel = ui_kit::fit_size(ctx, Vec2::new(PANEL_W, (screen.height() - 48.0).min(640.0)), Vec2::new(360.0, 300.0), 32.0);
        let (pw, h) = (panel.x, panel.y);
        egui::Window::new("Meu servidor")
            .id(egui::Id::new("host_panel"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .fixed_size(Vec2::new(pw, h))
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(ui_kit::dialog_frame(&ctx.global_style()).inner_margin(egui::Margin { left: 22, right: 10, top: 18, bottom: 12 }))
            .show(ctx, |ui| {
                ui.set_width(pw);
                // The head: icon, name, state, close.
                let (text, color) = phase_text(&st.phase);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = space::M;
                    ui.label(RichText::new(ph::HARD_DRIVES).font(theme::body(24.0)).color(theme::NEVOA));
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(RichText::new("Meu servidor").font(theme::body(12.5)).color(theme::NEVOA));
                        ui.add(egui::Label::new(RichText::new(&st.name).font(theme::display(size::HEADING)).color(theme::GIZ)).truncate().selectable(false));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                        ui.add_space(space::M);
                        if ui_kit::icon_button(ui, ph::X, "Fechar", IconState::Normal).clicked() {
                            open = false;
                        }
                        status_pill(ui, &text, color);
                    });
                });
                ui.add_space(space::L);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.set_width(pw - 14.0);
                    ui.spacing_mut().item_spacing.y = 6.0;
                    self.host_panel_body(ui, &st, mine);
                    ui.add_space(space::M);
                });
            });
        if !open {
            self.host.panel_open = false;
        }
    }

    fn host_panel_body(&mut self, ui: &mut egui::Ui, st: &HostStatus, mine: bool) {
        let label_w = 150.0;
        ui_kit::section(ui, "Endereços");
        ui_kit::group_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            let value = |ui: &mut egui::Ui, t: String| ui.label(RichText::new(t).color(theme::GIZ));
            let mono = |ui: &mut egui::Ui, t: String| ui.label(RichText::new(t).monospace().color(theme::GIZ));
            ui_kit::info_row(ui, label_w, "Portas", |ui| value(ui, st.ports.map(|p| p.describe()).unwrap_or_else(|| "...".into())));
            ui_kit::info_row(ui, label_w, "Na rede de casa", |ui| mono(ui, st.lan.map(|(ip, _)| ip.to_string()).unwrap_or_else(|| "?".into())));
            let public = match (st.public_ipv4(), st.public_v6) {
                (Some(v4), _) => v4.to_string(),
                (None, Some(v6)) => v6.to_string(),
                (None, None) => "não descoberto".into(),
            };
            ui_kit::info_row(ui, label_w, "Endereço público", |ui| mono(ui, public));
            let online = if mine { self.online.len().to_string() } else { "entre no servidor para ver".into() };
            ui_kit::info_row(ui, label_w, "Pessoas online", |ui| value(ui, online));
            ui_kit::info_row(ui, label_w, "Senha do servidor", |ui| {
                if self.host.show_password {
                    ui.label(RichText::new(&st.password).monospace().color(theme::GIZ));
                    if ui_kit::button_sized(ui, Kind::Ghost, Size::Small, Some(ph::COPY), "Copiar").clicked() {
                        ui.ctx().copy_text(st.password.clone());
                    }
                } else if ui_kit::button_sized(ui, Kind::Secondary, Size::Small, Some(ph::EYE), "Mostrar").on_hover_text("Para quem entra sem convite (o convite dispensa a senha)").clicked() {
                    self.host.show_password = true;
                }
            });
        });
        ui.add_space(space::L);
        // v1.1 (T0): the certificate's code and "Aceitar apps antigos sem conexão protegida".
        ui_kit::section(ui, crate::app::tls_ui::SECURITY_TITLE);
        ui_kit::group_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui_kit::info_row(ui, label_w, "Código do servidor", |ui| match st.tls_pin {
                Some(pin) => {
                    let code = vox_proto::tls_server_code(&pin);
                    ui.label(RichText::new(&code).monospace().color(theme::GIZ));
                    if ui_kit::button_sized(ui, Kind::Ghost, Size::Small, Some(ph::COPY), "Copiar").clicked() {
                        ui.ctx().copy_text(code);
                    }
                }
                None => {
                    ui.label(RichText::new("...").color(theme::NEVOA));
                }
            });
            let mut on = st.accept_old_apps;
            if ui.checkbox(&mut on, crate::app::tls_ui::OLD_APPS).changed() {
                if let Some(r) = &self.host.runner {
                    if let Err(e) = r.set_accept_old_apps(on) {
                        self.host.error = Some(format!("{e:#}"));
                    }
                }
            }
        });
        ui_kit::hint(
            ui,
            "Quem entra pelo concord 1.1 ou mais novo usa conexão protegida e pode conferir este código no menu do servidor, em \
             \"Segurança da conexão\". Ligado: apps antigos ainda entram, sem proteção. Desligado: eles recebem um aviso para atualizar.",
        );
        ui.add_space(space::L);
        ui_kit::section(ui, "Acesso pela internet");
        let (reach, rc) = reach_text(st);
        let kind = if rc == theme::ONLINE {
            None
        } else if rc == theme::AO_VIVO {
            Some(ui_kit::Notice::Error)
        } else {
            Some(ui_kit::Notice::Warning)
        };
        match kind {
            None => {
                ui_kit::group_frame().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui_kit::result_text(ui, true, &reach);
                });
            }
            Some(k) => notice(ui, k, &reach),
        }
        ui_kit::hint(ui, FIREWALL_TEXT);
        ui.add_space(space::L);
        ui_kit::section(ui, "Convites");
        if !mine {
            ui_kit::hint(ui, "Entre no seu servidor (\"Criar ou abrir servidor\", depois \"Entrar nele\") para criar convites.");
        } else if !self.my_role().can_admin() {
            ui_kit::hint(ui, "Defina sua senha pessoal (Configurações, Senha pessoal) para ligar as permissões de Dono e criar convites.");
        } else {
            ui_kit::hint(ui, INVITE_HINT);
            ui.add_space(space::S);
            self.invite_tools(ui);
        }
        ui.add_space(space::L);
        ui_kit::section(ui, "Desligar");
        ui_kit::group_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui_kit::button(ui, Kind::Danger, Some(ph::STOP), "Parar servidor").clicked() {
                        self.host.confirm_stop = true;
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui_kit::hint(ui, "Desliga o servidor: todo mundo cai, você também.");
                    });
                });
            });
        });
    }

    /// "Gerar convite", the code to copy, and the list with "Revogar" (host panel, and the server
    /// window's "Convites" page). Revoking asks first (`host_windows`).
    pub(crate) fn invite_tools(&mut self, ui: &mut egui::Ui) {
        if !self.host.inv.loaded && !self.host.inv.busy {
            self.invite_cmd(InviteCmd::List);
        }
        let inv_busy = self.host.inv.busy;
        ui_kit::group_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = space::M;
                ui.vertical(|ui| {
                    ui_kit::field_label(ui, "Validade");
                    egui::ComboBox::from_id_salt("inv_expiry").width(150.0).selected_text(EXPIRY[self.host.inv.expiry].1).show_ui(ui, |ui| {
                        for (i, (_, label)) in EXPIRY.iter().enumerate() {
                            ui.selectable_value(&mut self.host.inv.expiry, i, *label);
                        }
                    });
                });
                ui.vertical(|ui| {
                    ui_kit::field_label(ui, "Pode ser usado por");
                    egui::ComboBox::from_id_salt("inv_uses").width(150.0).selected_text(USES[self.host.inv.uses].1).show_ui(ui, |ui| {
                        for (i, (_, label)) in USES.iter().enumerate() {
                            ui.selectable_value(&mut self.host.inv.uses, i, *label);
                        }
                    });
                });
            });
            ui.add_space(space::S);
            if ui.add_enabled_ui(!inv_busy, |ui| ui_kit::button(ui, Kind::Primary, Some(ph::PLUS), "Gerar convite")).inner.clicked() {
                self.create_invite();
            }
            if let Some((_, code)) = self.host.inv.last.clone() {
                ui.add_space(space::M);
                ui_kit::body_text(ui, "Convite pronto! Mande este código para os amigos (WhatsApp, Discord...). Eles colam no campo \"Servidor ou convite\" do concord:");
                egui::Frame::new().fill(theme::FUNDO).corner_radius(theme::radius::ROW).inner_margin(egui::Margin::symmetric(10, 8)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.add(egui::Label::new(RichText::new(&code).monospace().color(theme::GIZ)).wrap());
                });
                ui.horizontal(|ui| {
                    if ui_kit::button(ui, Kind::Secondary, Some(ph::COPY), "Copiar convite").clicked() {
                        ui.ctx().copy_text(code.clone());
                        self.host.inv.copied_at = Some(Instant::now());
                    }
                    if let Some(t) = self.host.inv.copied_at {
                        let left = std::time::Duration::from_secs(3).saturating_sub(t.elapsed());
                        if !left.is_zero() {
                            ui_kit::result_text(ui, true, "Copiado!");
                            ui.ctx().request_repaint_after(left);
                        }
                    }
                });
            }
            if let Some(e) = &self.host.inv.error {
                ui_kit::result_text(ui, false, e);
            }
        });
        ui.add_space(space::M);
        ui.horizontal(|ui| {
            ui_kit::section(ui, "Convites criados");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add_enabled_ui(!inv_busy, |ui| ui_kit::icon_button(ui, ph::ARROWS_CLOCKWISE, "Atualizar a lista", IconState::Normal)).inner.clicked() {
                    self.invite_cmd(InviteCmd::List);
                }
            });
        });
        let now = crate::app::now_ms();
        let list = self.host.inv.list.clone();
        if list.is_empty() && self.host.inv.loaded {
            ui_kit::hint(ui, "Nenhum convite ainda.");
        }
        if !list.is_empty() {
            ui_kit::group_frame().inner_margin(egui::Margin::symmetric(6, 6)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 0.0;
                for i in &list {
                    self.invite_row(ui, i, now);
                }
            });
        }
        ui.add_space(space::S);
        ui_kit::hint(ui, HONEST_TEXT);
    }

    /// One invite: who made it, uses and validity; "Copiar" (codes made in this session) and
    /// "Revogar" while it is valid, its state otherwise.
    fn invite_row(&mut self, ui: &mut egui::Ui, i: &Invite, now: i64) {
        let state = i.state(now);
        let active = state == InviteState::Active;
        let uses = match i.max_uses {
            Some(m) => format!("usado {}/{m}", i.uses),
            None => format!("usado {}x", i.uses),
        };
        let until = match (state, i.expires_at_ms) {
            (InviteState::Revoked, _) => "revogado".to_string(),
            (InviteState::Expired, _) => "vencido".to_string(),
            (InviteState::UsedUp, _) => "esgotado".to_string(),
            (InviteState::Active, Some(e)) => format!("vale até {}", when_text(e)),
            (InviteState::Active, None) => "sem prazo".to_string(),
        };
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 44.0), Sense::hover());
        if resp.hovered() {
            ui.painter().rect_filled(rect, theme::radius::ROW, theme::ROW_HOVER);
        }
        let mut row = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(Vec2::new(8.0, 0.0))).layout(Layout::left_to_right(Align::Center)));
        let ui = &mut row;
        ui.spacing_mut().item_spacing.x = space::M;
        let fg = if active { theme::GIZ } else { theme::NEVOA };
        ui.label(RichText::new(ph::ENVELOPE_SIMPLE).font(theme::body(size::ICON)).color(theme::NEVOA));
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(RichText::new(format!("#{} por {}", i.id, i.created_by_name)).font(theme::semibold(13.5)).color(fg));
            ui.label(RichText::new(format!("{uses} · {until}")).font(theme::body(12.0)).color(theme::NEVOA));
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = space::S;
            if !active {
                ui_kit::pill(ui, &until, theme::MOVEL, theme::NEVOA);
                return;
            }
            if ui_kit::button_sized(ui, Kind::Secondary, Size::Small, None, "Revogar").on_hover_text("Ninguém mais entra com este convite (quem já entrou continua)").clicked() {
                self.host.inv.confirm_revoke = Some(i.id);
            }
            if let Some(code) = self.host.inv.codes.get(&i.id) {
                if ui_kit::button_sized(ui, Kind::Ghost, Size::Small, Some(ph::COPY), "Copiar").clicked() {
                    ui.ctx().copy_text(code.clone());
                }
            }
        });
    }
}

/// Width of the host panel's content.
const PANEL_W: f32 = 540.0;

/// What an invite does, said once above the invite tools (and as the server window's subtitle).
pub const INVITE_HINT: &str = "Quem tem um convite entra sem a senha do servidor. Depois, o computador da pessoa fica lembrado.";

/// The server's state: a dot in its colour and the text.
fn status_line(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (dot, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
        ui.painter().circle_filled(dot.center(), 4.0, color);
        ui.add(egui::Label::new(RichText::new(text).font(theme::semibold(12.5)).color(color)).wrap());
    });
}

/// The server's state as a pill (the panel's head).
fn status_pill(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    let fill = if color == theme::ONLINE { egui::Color32::from_rgb(0x1E, 0x3A, 0x2E) } else if color == theme::AO_VIVO { theme::PERIGO_FUNDO } else { theme::MOVEL };
    ui_kit::pill(ui, text, fill, color);
}

/// A notice in the kit's frame: its icon, the text.
fn notice(ui: &mut egui::Ui, kind: ui_kit::Notice, text: &str) {
    ui_kit::notice_frame(kind).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui_kit::notice_text(ui, kind, text);
    });
}

/// The voice server for a hosted server: `VOX_HOST_NO_LIVEKIT=1` runs without one (tests);
/// `VOX_HOST_LIVEKIT_BIN` names an exe to use in place (tests; still checked against the pin);
/// otherwise the installed copy, else `VOX_LIVEKIT_SOURCE` (a local folder with the release zip,
/// or the exe), else the download. Runs on its own thread.
fn prepare_voice_server(bin_dir: &std::path::Path, progress: &(dyn Fn(u64, Option<u64>) + Send + Sync)) -> Result<Option<PathBuf>, String> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    if env("VOX_HOST_NO_LIVEKIT").is_some_and(|v| v != "0") {
        return Ok(None);
    }
    if let Some(p) = env("VOX_HOST_LIVEKIT_BIN").map(PathBuf::from) {
        return match livekit_fetch::sha256_file(&p) {
            Ok(h) if h == livekit_fetch::PINNED.exe_sha256 => Ok(Some(p)),
            _ => Err("O servidor de voz indicado não confere com a assinatura esperada (SHA-256).".into()),
        };
    }
    if let Some(exe) = livekit_fetch::installed(bin_dir) {
        return Ok(Some(exe));
    }
    if let Some(src) = env("VOX_LIVEKIT_SOURCE").map(PathBuf::from) {
        return livekit_fetch::install_from(bin_dir, &src, &livekit_fetch::PINNED).map(Some).map_err(|e| format!("{e:#}"));
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(livekit_fetch::download(bin_dir, progress)).map(Some).map_err(|e| {
        tracing::error!(error = %format!("{e:#}"), "livekit download failed");
        // The first line of the chain is the message for people.
        format!("{e}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_addresses_from_what_the_client_knows() {
        assert_eq!(url_host_port("http://203.0.113.5:7890"), Some(("203.0.113.5".into(), Some(7890))));
        assert_eq!(url_host_port("http://[2001:db8::1]:7990/x"), Some(("2001:db8::1".into(), Some(7990))));
        assert_eq!(url_host_port("http://meu.example.org"), Some(("meu.example.org".into(), None)));
        let mut a = Vec::new();
        push_addr(&mut a, "127.0.0.1", 24);
        push_addr(&mut a, "localhost", 24);
        push_addr(&mut a, "192.168.0.20", 24);
        push_addr(&mut a, "203.0.113.5", 24);
        push_addr(&mut a, "203.0.113.5", 24);
        push_addr(&mut a, "Casa.Example.org", 24);
        push_addr(&mut a, "nao vale", 24);
        assert_eq!(
            a,
            [
                InviteAddr::Lan("192.168.0.20".parse().unwrap(), 24),
                InviteAddr::Public("203.0.113.5".parse().unwrap()),
                InviteAddr::Host("casa.example.org".into())
            ]
        );
    }

    #[test]
    fn code_errors_read_in_portuguese() {
        for e in [InviteCodeError::Typo, InviteCodeError::NewerVersion, InviteCodeError::Damaged, InviteCodeError::NotAnInvite] {
            assert!(!code_error_text(e).is_empty());
        }
        assert!(code_error_text(InviteCodeError::Typo).contains("vox1-"));
    }
}
