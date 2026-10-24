//! "Seus servidores salvos" (v0.9.1): the saved server list at the top of "Conectar".
//!
//! - Every successful login (a typed address, an invite code, or my own hosted server through
//!   127.0.0.1) saves or updates an entry, unless "Salvar na lista de servidores" is off. Entries
//!   are told apart by normalized address + port + name, case-insensitively ([`server_key`]).
//! - One click on an entry logs in with what this computer kept for it: the server password
//!   (DPAPI) and/or this computer's login token ("Lembrar este computador"). When something is
//!   missing the form opens filled in and asks only for that ([`plan`]).
//! - The list lives in `servers.json`, next to `settings.json` ([`ServerList`]). v0.9.0 rewrites
//!   `settings.json` with only the fields it knows, so a file of its own survives a downgrade. The
//!   old single-server fields of `settings.json` keep mirroring the most recently used entry
//!   ([`Settings::sync_legacy_server`]): v0.9.0 still finds its login, and the server it last
//!   used joins the list again on the next start ([`Settings::merge_legacy_server`], which is also
//!   the first-run migration).
//! - Secrets: an entry's server password is DPAPI-encrypted (like `password_enc` always was). This
//!   computer's login token stays in `Settings::device_tokens`, under the entry's own
//!   `address|name` key: one per entry, and where v0.9.0 looks for it. Removing an entry deletes
//!   both. Neither is ever logged.
//! - The server's identity (v1.0.1, [`SavedServer::server_id`], and the token's own in
//!   `Settings::device_token_servers`): the saved password and the token are sent only to an
//!   address that answers as that server (`backend::Guard`). An entry saved before has none until
//!   its next login at the address the user typed or picked (or through an accepted invite).
//! - The server's certificate (v1.1, [`SavedServer::tls_pin`], and the token's own in
//!   `Settings::device_token_pins`): once known, the app talks to the entry only over TLS with that
//!   certificate, never in the clear (`backend/tls.rs`, the trust rules). The address stays
//!   `http://host:port`: the pin decides the transport.
//! - The home screen asks every entry's hub for `GET /api/health` in the background ([`Pinger`]):
//!   at most every 30 s, only while the home screen shows, on a thread of its own with a short
//!   timeout (the UI never waits for it).

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, Layout, RichText, Sense};
use serde::{Deserialize, Serialize};
use vox_client_core::hub::HubClient;
use vox_proto::{InviteAddr, InviteCode, INVITE_TOKEN_LEN};
use voxhub_server::host::{self as hosting, Hosted};

use crate::app::{App, CameraPreviewUi, MicTestUi, Screen, GREEN};
use crate::backend::{normalize_url, Cmd};
use crate::host::HomeTab;
use crate::settings::{protect, unprotect, Settings};

/// The list keeps at most this many entries (the least recently used one goes).
pub const MAX_SERVERS: usize = 50;
/// The home screen checks the saved servers at most this often.
pub const PING_EVERY: Duration = Duration::from_secs(30);
/// One health check gives up after this long.
const PING_TIMEOUT: Duration = Duration::from_secs(4);
const LIST_W: f32 = 420.0;
const ROW_H: f32 = 60.0;
const OFFLINE_GRAY: Color32 = crate::theme::DESLIGADO;

// ---------------------------------------------------------------- data

/// One of an invite's addresses, kept with the entry (never the invite's token): the client can
/// still try the LAN address first when it is on the host's network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AltAddr {
    Lan { ip: Ipv4Addr, prefix: u8 },
    Public { ip: IpAddr },
    Host { name: String },
}

impl From<&InviteAddr> for AltAddr {
    fn from(a: &InviteAddr) -> Self {
        match a {
            InviteAddr::Lan(ip, prefix) => AltAddr::Lan { ip: *ip, prefix: *prefix },
            InviteAddr::Public(ip) => AltAddr::Public { ip: *ip },
            InviteAddr::Host(name) => AltAddr::Host { name: name.clone() },
        }
    }
}

impl AltAddr {
    fn to_invite(&self) -> InviteAddr {
        match self {
            AltAddr::Lan { ip, prefix } => InviteAddr::Lan(*ip, *prefix),
            AltAddr::Public { ip } => InviteAddr::Public(*ip),
            AltAddr::Host { name } => InviteAddr::Host(name.clone()),
        }
    }

    fn url(&self, port: u16) -> String {
        match self {
            AltAddr::Lan { ip, .. } => format!("http://{ip}:{port}"),
            AltAddr::Public { ip: IpAddr::V4(v4) } => format!("http://{v4}:{port}"),
            AltAddr::Public { ip: IpAddr::V6(v6) } => format!("http://[{v6}]:{port}"),
            AltAddr::Host { name } => format!("http://{name}:{port}"),
        }
    }
}

/// A saved server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedServer {
    /// Local id, never reused.
    pub id: u64,
    /// The address that worked last (normalized: `http://host:port`).
    pub url: String,
    pub username: String,
    /// The name the hub gave at the last login.
    pub server_name: String,
    /// "Renomear": a local nickname (empty: the hub's name).
    pub nickname: String,
    /// DPAPI-encrypted server password, hex encoded ("Lembrar a senha").
    pub password_enc: Option<String>,
    /// The last login typed a server password (so one is needed when there is no device token).
    pub needs_password: bool,
    /// The name has a personal password on this hub.
    pub personal: bool,
    /// The invite's hub port and addresses, when the entry came from an invite.
    pub alt_port: u16,
    pub alt_addrs: Vec<AltAddr>,
    /// My own hosted server (`Hosted::id`): one click opens it.
    pub hosted: Option<String>,
    /// Last successful login (Unix ms; 0 = unknown).
    pub last_used_ms: i64,
    /// v1.0.1: the server's identity (`HealthResponse::server_id`), empty while unknown (an entry
    /// saved before, or an older hub). The saved password goes only to an address that answers
    /// with it. Not a secret.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub server_id: String,
    /// v1.1 (T0): the SHA-256 of the server's TLS certificate, 64 hex digits, empty while unknown
    /// (an older hub, or not logged in since). With it: pinned TLS only, never plain HTTP. Not a
    /// secret.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tls_pin: String,
}

/// The dedup key: normalized address + port (+ path) and name, case-insensitive. The same text
/// keys the entry's device token in `Settings::device_tokens`.
pub fn server_key(url: &str, username: &str) -> String {
    format!("{}|{}", normalize_url(url).to_ascii_lowercase(), username.trim().to_ascii_lowercase())
}

/// `http://203.0.113.5:7890` -> `203.0.113.5:7890`.
fn shown_address(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.trim_end_matches('/').to_string()
}

impl SavedServer {
    pub fn key(&self) -> String {
        server_key(&self.url, &self.username)
    }

    /// The name on the row: the nickname, else the hub's name, else the address.
    pub fn title(&self) -> String {
        [self.nickname.trim(), self.server_name.trim()].into_iter().find(|s| !s.is_empty()).map_or_else(|| shown_address(&self.url), str::to_string)
    }

    /// The addresses to try, in order: an invite's (the LAN address first when this computer is on
    /// that network, see `InviteCode::candidate_urls`), then the one that worked last.
    pub fn candidate_urls(&self, local: &[(IpAddr, u8)]) -> Vec<String> {
        let mut out = if self.alt_port != 0 && !self.alt_addrs.is_empty() {
            let code = InviteCode { token: [0u8; INVITE_TOKEN_LEN], hub_port: self.alt_port, addrs: self.alt_addrs.iter().map(AltAddr::to_invite).collect(), tls_pin: None };
            code.candidate_urls(local)
        } else {
            Vec::new()
        };
        let url = normalize_url(&self.url);
        if !out.iter().any(|u| normalize_url(u) == url) {
            out.push(url);
        }
        out
    }

    /// The server's identity, if known.
    pub fn server_id(&self) -> Option<String> {
        Some(self.server_id.clone()).filter(|id| vox_proto::is_server_id(id))
    }

    /// v1.1 (T0): the server's certificate pin, if known.
    pub fn tls_pin(&self) -> Option<[u8; 32]> {
        vox_proto::tls_pin_from_hex(&self.tls_pin)
    }

    /// Every address this entry may keep a login token under.
    fn all_urls(&self) -> Vec<String> {
        let mut out = vec![normalize_url(&self.url)];
        for a in &self.alt_addrs {
            let u = normalize_url(&a.url(self.alt_port));
            if !out.contains(&u) {
                out.push(u);
            }
        }
        out
    }
}

/// `servers.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerList {
    /// The next local id (ids are never reused, even after a removal).
    pub next_id: u64,
    /// In the user's order.
    pub servers: Vec<SavedServer>,
}

#[derive(Serialize)]
struct ServersFile<'a> {
    version: u32,
    next_id: u64,
    servers: &'a [SavedServer],
}

impl ServerList {
    /// Reads `servers.json` leniently: an entry this version cannot read is skipped, the rest is
    /// kept. `None` when the text is not a list file at all.
    pub fn from_json(text: &str) -> Option<ServerList> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let obj = v.as_object()?;
        let mut servers: Vec<SavedServer> = obj
            .get("servers")
            .and_then(|s| s.as_array())
            .map(|a| a.iter().filter_map(|e| serde_json::from_value(e.clone()).ok()).collect())
            .unwrap_or_default();
        let mut keys = HashSet::new();
        servers.retain(|s| !s.url.trim().is_empty() && !s.username.trim().is_empty() && keys.insert(s.key()));
        servers.truncate(MAX_SERVERS);
        let max_id = servers.iter().map(|s| s.id).max().unwrap_or(0);
        let mut next = obj.get("next_id").and_then(|n| n.as_u64()).unwrap_or(0).max(max_id + 1);
        // A hand-edited file with a missing or repeated id: fresh ones.
        let mut ids = HashSet::new();
        for s in &mut servers {
            if s.id == 0 || !ids.insert(s.id) {
                s.id = next;
                ids.insert(next);
                next += 1;
            }
        }
        Some(ServerList { next_id: next, servers })
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&ServersFile { version: 1, next_id: self.next_id, servers: &self.servers }).unwrap_or_default()
    }

    pub fn get(&self, id: u64) -> Option<&SavedServer> {
        self.servers.iter().find(|s| s.id == id)
    }

    fn pos(&self, id: u64) -> Option<usize> {
        self.servers.iter().position(|s| s.id == id)
    }

    fn pos_key(&self, key: &str) -> Option<usize> {
        self.servers.iter().position(|s| s.key() == key)
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        id
    }

    /// The least recently used entry other than `keep`.
    fn oldest_except(&self, keep: u64) -> Option<u64> {
        self.servers.iter().filter(|s| s.id != keep).min_by_key(|s| s.last_used_ms).map(|s| s.id)
    }
}

/// A login that went through, for [`Settings::remember_server`].
pub struct Login<'a> {
    /// The address that worked.
    pub url: &'a str,
    pub username: &'a str,
    pub server_name: &'a str,
    /// The server password used, and whether to keep it ("Lembrar a senha").
    pub password: &'a str,
    pub remember: bool,
    /// A personal password was typed for this login.
    pub personal_typed: bool,
    /// A saved device token was sent.
    pub device_used: bool,
    /// The entry this login came from (one click on the list).
    pub entry: Option<u64>,
    /// The invite's port and addresses, when the login used one.
    pub invite: Option<(u16, Vec<AltAddr>)>,
    /// My own hosted server (`Hosted::id`).
    pub hosted: Option<String>,
    /// v1.0.1: the server's identity, when this login confirmed it (`Ev::LoggedIn::pin`).
    pub server_id: Option<&'a str>,
    /// v1.1 (T0): the server's certificate, when this login may keep it (pinned TLS, or seen for
    /// the first time on a login that confirmed the server).
    pub tls_pin: Option<[u8; 32]>,
    pub now_ms: i64,
}

impl Settings {
    pub fn server_entry(&self, id: u64) -> Option<&SavedServer> {
        self.servers.get(id)
    }

    /// An entry's saved server password, decrypted.
    pub fn server_password(&self, e: &SavedServer) -> Option<String> {
        unprotect(e.password_enc.as_deref()?).filter(|p| !p.is_empty())
    }

    /// This computer's login token for an entry (under its address, else an invite's other one).
    pub fn server_device_token(&self, e: &SavedServer) -> Option<String> {
        self.server_device_token_at(e).map(|(_, t)| t)
    }

    /// [`Settings::server_device_token`] with the address it is saved under.
    pub fn server_device_token_at(&self, e: &SavedServer) -> Option<(String, String)> {
        e.all_urls().into_iter().find_map(|u| self.device_token(&u, &e.username).map(|t| (u, t)))
    }

    /// Move a login token (with its server identity) to another address / name (an entry whose
    /// address changed).
    fn move_device_token(&mut self, from_url: &str, from_user: &str, to_url: &str, to_user: &str) {
        if server_key(from_url, from_user) == server_key(to_url, to_user) {
            return;
        }
        let (from, to) = (normalize_url(from_url), normalize_url(to_url));
        if let Some(t) = self.device_token(&from, from_user) {
            if self.device_token(&to, to_user).is_none() {
                let id = self.device_token_server(&from, from_user);
                let pin = self.device_token_pin(&from, from_user);
                self.set_device_token(&to, to_user, Some(&t));
                self.set_device_token_server(&to, to_user, id.as_deref());
                self.set_device_token_pin(&to, to_user, pin);
            }
            self.set_device_token(&from, from_user, None);
        }
    }

    /// The addresses whose login token for `e`'s name belongs to `e`: its own, and an invite's
    /// other addresses unless another entry of the same name lives there (v1.0.1: removing one
    /// entry no longer deletes another entry's token).
    fn owned_token_urls(&self, e: &SavedServer) -> Vec<String> {
        let own = normalize_url(&e.url);
        let others: Vec<String> = self.servers.servers.iter().filter(|o| o.id != e.id && o.username.trim().eq_ignore_ascii_case(e.username.trim())).map(|o| normalize_url(&o.url).to_ascii_lowercase()).collect();
        e.all_urls().into_iter().filter(|u| *u == own || !others.contains(&u.to_ascii_lowercase())).collect()
    }

    /// Save or update the entry of a login that went through; returns its id.
    pub fn remember_server(&mut self, l: Login<'_>) -> u64 {
        let url = normalize_url(l.url);
        let user = l.username.trim().to_string();
        let key = server_key(&url, &user);
        let by_entry = l.entry.and_then(|id| self.servers.pos(id));
        let by_key = self.servers.pos_key(&key);
        let mut at = by_entry.or(by_key);
        if let (Some(a), Some(b)) = (by_entry, by_key) {
            if a != b {
                // The entry's address turned into one already saved under the same name: one entry.
                self.servers.servers.remove(b);
                at = Some(if b < a { a - 1 } else { a });
            }
        }
        let id = match at {
            Some(i) => {
                let (old_url, old_user) = (self.servers.servers[i].url.clone(), self.servers.servers[i].username.clone());
                self.move_device_token(&old_url, &old_user, &url, &user);
                self.servers.servers[i].id
            }
            None => {
                let id = self.servers.alloc_id();
                self.servers.servers.push(SavedServer { id, ..Default::default() });
                id
            }
        };
        let enc = if l.remember && !l.password.is_empty() { protect(l.password) } else { None };
        if let Some(e) = self.servers.servers.iter_mut().find(|s| s.id == id) {
            e.url = url;
            e.username = user;
            if !l.server_name.trim().is_empty() {
                e.server_name = l.server_name.trim().to_string();
            }
            e.password_enc = enc;
            if !l.password.is_empty() {
                e.needs_password = true;
            } else if !l.device_used {
                e.needs_password = false;
            }
            e.personal |= l.personal_typed;
            if let Some((port, addrs)) = l.invite {
                e.alt_port = port;
                e.alt_addrs = addrs;
            }
            if l.hosted.is_some() {
                e.hosted = l.hosted;
            }
            if let Some(id) = l.server_id.filter(|id| vox_proto::is_server_id(id)) {
                e.server_id = id.to_string();
            }
            if let Some(pin) = l.tls_pin {
                e.tls_pin = vox_proto::tls_pin_hex(&pin);
            }
            e.last_used_ms = l.now_ms;
        }
        while self.servers.servers.len() > MAX_SERVERS {
            let Some(old) = self.servers.oldest_except(id) else { break };
            self.remove_server(old);
        }
        self.sync_legacy_server();
        id
    }

    /// "Remover da lista": the entry goes, with its saved password and this computer's login
    /// token for it.
    pub fn remove_server(&mut self, id: u64) -> bool {
        let Some(e) = self.servers.get(id).cloned() else { return false };
        for u in self.owned_token_urls(&e) {
            self.set_device_token(&u, &e.username, None);
        }
        self.servers.servers.retain(|s| s.id != id);
        self.sync_legacy_server();
        true
    }

    /// The hub no longer knows this computer for an entry: its login tokens go.
    pub fn forget_server_device(&mut self, id: u64) {
        if let Some(e) = self.servers.get(id).cloned() {
            for u in self.owned_token_urls(&e) {
                self.set_device_token(&u, &e.username, None);
            }
        }
    }

    /// "Esquecer senha": the saved server password of an entry.
    pub fn forget_server_password(&mut self, id: u64) {
        if let Some(e) = self.servers.servers.iter_mut().find(|s| s.id == id) {
            e.password_enc = None;
        }
        self.sync_legacy_server();
    }

    /// v1.1 (T0): "Confiar no novo código": the entries of this address and name, and the login
    /// tokens saved there, take the certificate `pin` from now on.
    pub fn trust_new_pin(&mut self, url: &str, username: &str, pin: [u8; 32]) {
        let key = server_key(url, username);
        let hex = vox_proto::tls_pin_hex(&pin);
        let mut urls = vec![normalize_url(url)];
        for e in self.servers.servers.iter_mut().filter(|e| e.key() == key || (e.username.trim().eq_ignore_ascii_case(username.trim()) && e.all_urls().contains(&normalize_url(url)))) {
            e.tls_pin = hex.clone();
            urls.extend(e.all_urls());
        }
        for u in urls {
            if self.device_token(&u, username).is_some() {
                self.set_device_token_pin(&u, username, Some(pin));
            }
        }
    }

    /// "Renomear": a local nickname (empty: back to the hub's name).
    pub fn rename_server(&mut self, id: u64, nickname: &str) {
        if let Some(e) = self.servers.servers.iter_mut().find(|s| s.id == id) {
            e.nickname = nickname.trim().chars().take(60).collect();
        }
    }

    /// "Editar endereço". The login token follows the entry; an invite's other addresses and the
    /// hosted-server link no longer apply.
    pub fn edit_server_address(&mut self, id: u64, text: &str) -> Result<(), String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("Informe o endereço do servidor.".into());
        }
        if vox_proto::looks_like_invite(text) {
            return Err("Aqui vai o endereço. Um código de convite entra pelo formulário \"Adicionar servidor\".".into());
        }
        let Some(e) = self.servers.get(id).cloned() else { return Err("Esse servidor não está mais na lista.".into()) };
        let url = normalize_url(text);
        let key = server_key(&url, &e.username);
        if self.servers.servers.iter().any(|o| o.id != id && o.key() == key) {
            return Err("Esse endereço já está na lista com esse nome.".into());
        }
        if key != e.key() {
            // Audit (T0): the token's certificate pin goes along too (it used to be dropped here,
            // and a token without a pin may go to a server without TLS).
            let token = self.server_device_token_at(&e).map(|(at, t)| (self.device_token_server(&at, &e.username), self.device_token_pin(&at, &e.username), t));
            for u in self.owned_token_urls(&e) {
                self.set_device_token(&u, &e.username, None);
            }
            if let Some((server, pin, t)) = token {
                self.set_device_token(&url, &e.username, Some(&t));
                self.set_device_token_server(&url, &e.username, server.as_deref());
                self.set_device_token_pin(&url, &e.username, pin);
            }
        }
        if let Some(s) = self.servers.servers.iter_mut().find(|s| s.id == id) {
            s.url = url;
            s.alt_port = 0;
            s.alt_addrs.clear();
            s.hosted = None;
        }
        self.sync_legacy_server();
        Ok(())
    }

    /// "Mover para cima / baixo".
    pub fn move_server(&mut self, id: u64, up: bool) {
        let Some(i) = self.servers.pos(id) else { return };
        let j = if up { i.checked_sub(1) } else { Some(i + 1).filter(|j| *j < self.servers.servers.len()) };
        if let Some(j) = j {
            self.servers.servers.swap(i, j);
        }
    }

    /// The name's personal password was set or removed while logged in.
    pub fn set_server_personal(&mut self, url: &str, username: &str, on: bool) {
        let key = server_key(url, username);
        if let Some(e) = self.servers.servers.iter_mut().find(|s| s.key() == key) {
            e.personal = on;
        }
    }

    /// v0.9.0's single-server fields mirror the most recently used entry (cleared with the list).
    pub fn sync_legacy_server(&mut self) {
        let latest = self.servers.servers.iter().enumerate().max_by_key(|(i, s)| (s.last_used_ms, std::cmp::Reverse(*i))).map(|(_, s)| s.clone());
        match latest {
            Some(s) => {
                self.server = s.url;
                self.username = s.username;
                self.remember_password = s.password_enc.is_some();
                self.password_enc = s.password_enc;
            }
            None => {
                self.server.clear();
                self.username.clear();
                self.remember_password = false;
                self.password_enc = None;
            }
        }
    }

    /// The single saved server of v0.9.0 (or of a v0.9.0 run after a downgrade) joins the list,
    /// as its first entry. Returns whether it did.
    pub fn merge_legacy_server(&mut self, now_ms: i64) -> bool {
        let (server, user) = (self.server.trim().to_string(), self.username.trim().to_string());
        if server.is_empty() || user.is_empty() || vox_proto::looks_like_invite(&server) {
            return false;
        }
        let url = normalize_url(&server);
        if self.servers.pos_key(&server_key(&url, &user)).is_some() {
            return false;
        }
        let password_enc = if self.remember_password { self.password_enc.clone().filter(|p| unprotect(p).is_some_and(|p| !p.is_empty())) } else { None };
        // First run: nobody knows when it was used. After a downgrade: it is the newest login.
        let last_used_ms = if self.servers.servers.is_empty() { 0 } else { now_ms };
        let id = self.servers.alloc_id();
        let e = SavedServer { id, url, username: user, needs_password: password_enc.is_some(), password_enc, last_used_ms, ..Default::default() };
        self.servers.servers.insert(0, e);
        while self.servers.servers.len() > MAX_SERVERS {
            let Some(old) = self.servers.oldest_except(id) else { break };
            self.remove_server(old);
        }
        true
    }
}

// ---------------------------------------------------------------- decisions and texts

/// What one click on an entry does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Log in right away with what is saved.
    Join,
    /// My own hosted server: open it (or enter it, when it already runs).
    Host,
    /// The form opens filled in and asks for the server password.
    AskPassword,
    /// The form opens filled in and asks for the personal password.
    AskPersonal,
}

/// The one-click decision. A server password that the last login needed must be saved (people
/// who came with an invite never needed one: their login token lets them in, see the hub's
/// `invites::admit`). A protected name needs this computer's login token, else its personal
/// password.
pub fn plan(e: &SavedServer, has_password: bool, has_device: bool, hosted_here: bool) -> Plan {
    if e.hosted.is_some() && hosted_here {
        Plan::Host
    } else if e.needs_password && !has_password {
        Plan::AskPassword
    } else if e.personal && !has_device {
        Plan::AskPersonal
    } else {
        Plan::Join
    }
}

/// "agora há pouco", "hoje", "ontem", "há 3 dias", "há 2 semanas", "há 5 meses", "há 1 ano"
/// (calendar days in local time); empty when unknown.
pub fn last_used_text(then_ms: i64, now_ms: i64, offset_min: i32) -> String {
    if then_ms <= 0 {
        return String::new();
    }
    let off = offset_min as i64 * 60_000;
    let day = |ms: i64| (ms + off).div_euclid(86_400_000);
    let days = day(now_ms) - day(then_ms);
    let plural = |n: i64, one: &str, many: &str| if n == 1 { format!("há 1 {one}") } else { format!("há {n} {many}") };
    match days {
        i64::MIN..=0 if now_ms - then_ms < 3_600_000 => "agora há pouco".into(),
        i64::MIN..=0 => "hoje".into(),
        1 => "ontem".into(),
        2..=6 => format!("há {days} dias"),
        7..=29 => plural(days / 7, "semana", "semanas"),
        30..=364 => plural(days / 30, "mês", "meses"),
        _ => plural(days / 365, "ano", "anos"),
    }
}

// ---------------------------------------------------------------- the background check

/// The home screen's online / offline dots.
#[derive(Default)]
pub struct Pinger {
    rx: Option<Receiver<Vec<(u64, bool)>>>,
    last: Option<Instant>,
    /// Entry id -> its hub answered the last check.
    pub online: HashMap<u64, bool>,
    /// Rounds started so far (the autotest checks that none starts while logged in).
    pub rounds: u32,
}

impl Pinger {
    pub fn busy(&self) -> bool {
        self.rx.is_some()
    }

    /// Take a finished round's answers; true when some came in.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else { return false };
        match rx.try_recv() {
            Ok(res) => {
                self.online.extend(res);
                self.rx = None;
                self.last = Some(Instant::now());
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.rx = None;
                self.last = Some(Instant::now());
                false
            }
        }
    }

    /// Time left before the next round (zero: due now).
    pub fn due_in(&self, now: Instant) -> Duration {
        self.last.map_or(Duration::ZERO, |t| PING_EVERY.saturating_sub(now.saturating_duration_since(t)))
    }

    /// Check `targets` (entry id, addresses in order, the certificate pin when known) on a thread
    /// of its own; `wake` repaints.
    pub fn start(&mut self, targets: Vec<(u64, Vec<String>, Option<[u8; 32]>)>, wake: impl Fn() + Send + 'static) {
        let (tx, rx) = channel();
        let spawned = std::thread::Builder::new().name("server-ping".into()).spawn(move || {
            let _ = tx.send(ping_all(targets));
            wake();
        });
        self.rounds += 1;
        match spawned {
            Ok(_) => self.rx = Some(rx),
            Err(_) => self.last = Some(Instant::now()),
        }
    }

    /// Not on the home screen any more: a round in flight ends on its own and its answer is dropped.
    pub fn stop(&mut self) {
        self.rx = None;
    }

    /// Check again at the next chance (back on the home screen after "Trocar de servidor").
    pub fn soon(&mut self) {
        self.last = None;
    }
}

fn ping_all(targets: Vec<(u64, Vec<String>, Option<[u8; 32]>)>) -> Vec<(u64, bool)> {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        return targets.into_iter().map(|(id, _, _)| (id, false)).collect();
    };
    rt.block_on(async move {
        let mut set = tokio::task::JoinSet::new();
        for (id, urls, pin) in targets {
            set.spawn(async move {
                for u in urls {
                    if answers(&u, pin).await {
                        return (id, true);
                    }
                }
                (id, false)
            });
        }
        let mut out = Vec::new();
        while let Some(r) = set.join_next().await {
            if let Ok(r) = r {
                out.push(r);
            }
        }
        out
    })
}

/// v1.1 (T0): a pinned entry is asked over TLS with its certificate only (another one shows as
/// offline; a click then says why); one without a pin over TLS when the hub has it, else in the
/// clear as before.
async fn answers(url: &str, pin: Option<[u8; 32]>) -> bool {
    let pin = match pin {
        Some(p) => Some(p),
        None => match tokio::time::timeout(PING_TIMEOUT, vox_client_core::tls::probe_url(url)).await {
            Ok(vox_client_core::tls::Probe::Tls(p)) => Some(p),
            Ok(vox_client_core::tls::Probe::NoTls) => None,
            _ => return false,
        },
    };
    let hub = match pin {
        Some(p) => HubClient::new_pinned(url, p),
        None => HubClient::new(url),
    };
    let Ok(hub) = hub else { return false };
    matches!(tokio::time::timeout(PING_TIMEOUT, hub.health()).await, Ok(Ok(h)) if h.ok)
}

// ---------------------------------------------------------------- UI state

/// An inline editor under a row.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Edit {
    Rename(u64, String),
    Address(u64, String, Option<String>),
    Remove(u64),
}

#[derive(Default)]
pub struct ServersUi {
    /// The form under the list ("+ Adicionar servidor") is open.
    pub form_open: bool,
    /// The entry being joined from the list (the login updates it).
    pub joining: Option<u64>,
    /// The login in flight sent a saved device token.
    device_used: bool,
    /// The invite's addresses of the login in flight (kept with the entry; never its token).
    pending_invite: Option<(u16, Vec<AltAddr>)>,
    edit: Option<Edit>,
    pub pinger: Pinger,
    /// Put the cursor in the server password once (the form opened asking for it).
    pub focus_password: bool,
}

impl ServersUi {
    /// With saved servers the list leads; the form is "+ Adicionar servidor".
    pub fn new(form_open: bool) -> Self {
        ServersUi { form_open, ..Default::default() }
    }
}

/// What a click on the list asks for (applied after drawing).
enum Act {
    Join(u64),
    Add,
    CloseForm,
    Rename(u64),
    EditAddress(u64),
    ForgetPassword(u64),
    AskRemove(u64),
    Move(u64, bool),
    SaveEdit,
    Remove(u64),
    CancelEdit,
}

/// The badge's colour (v1.0: the mockup's soft pastels; the letter is dark on them).
fn badge_color(text: &str) -> Color32 {
    const COLORS: [Color32; 6] = [
        Color32::from_rgb(0xB5, 0xD6, 0x9C),
        Color32::from_rgb(0x9F, 0xD3, 0xC7),
        Color32::from_rgb(0xC9, 0xB8, 0xF2),
        Color32::from_rgb(0xE7, 0xA6, 0xC1),
        Color32::from_rgb(0xF2, 0xD2, 0x9B),
        Color32::from_rgb(0x9C, 0xC4, 0xEF),
    ];
    let h = text.to_lowercase().bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    COLORS[h as usize % COLORS.len()]
}

// ---------------------------------------------------------------- the app side

impl App {
    /// Every frame: the online / offline check, only while the home screen (with a list) shows.
    pub(crate) fn servers_tick(&mut self, ctx: &egui::Context) {
        let home = self.screen == Screen::Connect && !self.notify.hidden && !self.settings.servers.servers.is_empty();
        if !home {
            self.servers.pinger.stop();
            return;
        }
        // My own server, opened from the list, did not start: say so here (not on the other tab).
        let opening_mine = self.servers.joining.and_then(|id| self.settings.server_entry(id)).is_some_and(|e| e.hosted.is_some());
        if opening_mine && self.host.tab == HomeTab::Connect {
            if let Some(err) = self.host.error.take() {
                self.form.error = Some(err);
                self.servers.form_open = true;
                self.servers.joining = None;
            }
        }
        self.servers.pinger.poll();
        if self.servers.pinger.busy() {
            return;
        }
        let wait = self.servers.pinger.due_in(Instant::now());
        if !wait.is_zero() {
            ctx.request_repaint_after(wait);
            return;
        }
        let local = crate::host::local_addrs();
        let targets: Vec<(u64, Vec<String>, Option<[u8; 32]>)> = self.settings.servers.servers.iter().map(|e| (e.id, e.candidate_urls(&local), e.tls_pin())).collect();
        let r = self.repaint.clone();
        self.servers.pinger.start(targets, move || r.wake());
    }

    /// The hosted server `id` of mine, if it still exists.
    fn hosted_by_id(&mut self, id: &str) -> Option<Hosted> {
        if let Some(h) = self.host.hosted.clone().filter(|h| h.id == id) {
            return Some(h);
        }
        hosting::list(&self.host.root).into_iter().find(|h| h.id == id)
    }

    /// The hosted server this login went into (through 127.0.0.1), if any.
    fn hosted_login(&self, url: &str) -> Option<String> {
        let port = self.host.status()?.hub_addr?.port();
        let (h, p) = crate::host::url_host_port(url)?;
        let loopback = h.eq_ignore_ascii_case("localhost") || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
        if loopback && p == Some(port) {
            self.host.hosted.as_ref().map(|h| h.id.clone())
        } else {
            None
        }
    }

    /// One click on a saved server: log in with what is saved, or open the form asking only for
    /// what is missing.
    pub fn join_saved(&mut self, id: u64) {
        if self.form.busy {
            return;
        }
        let Some(e) = self.settings.server_entry(id).cloned() else { return };
        let password = self.settings.server_password(&e);
        let device = self.settings.server_device_token(&e).is_some();
        let hosted = e.hosted.clone().and_then(|h| self.hosted_by_id(&h));
        self.form.url = e.url.clone();
        self.form.user = e.username.clone();
        // M1 (Mac): an entry whose remembered password cannot be read now (Keychain item gone or
        // access denied, or another copy of the app rewrote the files) keeps "Lembrar a senha"
        // ticked, so typing it once saves it again instead of silently forgetting it for good.
        self.form.remember = password.is_some() || e.password_enc.is_some();
        self.form.password = password.unwrap_or_default();
        self.form.personal.clear();
        self.form.need_personal = false;
        self.form.error = None;
        // A click on the list sends what is saved, so the identity check applies again.
        self.form.manual = None;
        self.servers.joining = Some(id);
        self.servers.edit = None;
        self.host.tab = HomeTab::Connect;
        let plan = plan(&e, !self.form.password.is_empty(), device, hosted.is_some());
        tracing::info!(id, url = %e.url, saved_password = !self.form.password.is_empty(), device_token = device, ?plan, "saved server clicked");
        let title = e.title();
        match plan {
            Plan::Join => self.connect(),
            Plan::Host => {
                let Some(h) = hosted else { return };
                self.host.error = None;
                if self.host.hosting() && self.host.hosted.as_ref().is_some_and(|x| x.id == h.id) {
                    self.host.auto_login = true;
                } else {
                    self.host_open(h);
                    if let Some(err) = self.host.error.take() {
                        self.form.error = Some(err);
                        self.servers.form_open = true;
                    }
                }
            }
            Plan::AskPassword => {
                self.servers.form_open = true;
                self.servers.focus_password = true;
                self.form.error = Some(format!("Digite a senha do servidor \"{title}\" para entrar (ela não está salva neste computador)."));
            }
            Plan::AskPersonal => {
                self.servers.form_open = true;
                self.form.need_personal = true;
                self.form.remember_device = true;
                self.form.error = Some(format!("Digite a senha pessoal de \"{}\" para entrar em \"{title}\" (este computador não está lembrado).", e.username));
            }
        }
    }

    /// `connect()` is about to log in to `url` as `user`: the entry being joined still applies?
    /// Returns the addresses to try (the entry's, LAN first), empty for just `url`.
    pub(crate) fn servers_before_connect(&mut self, url: &str, user: &str) -> Vec<String> {
        self.servers.pending_invite = None;
        self.servers.device_used = false;
        let Some(e) = self.servers.joining.and_then(|id| self.settings.server_entry(id)).cloned() else {
            self.servers.joining = None;
            return Vec::new();
        };
        let same_name = e.username.trim().eq_ignore_ascii_case(user.trim());
        let same_place = !vox_proto::looks_like_invite(url) && e.all_urls().contains(&normalize_url(url));
        if !(same_name && same_place) {
            // The form now names another server: a new entry.
            self.servers.joining = None;
            return Vec::new();
        }
        let urls = e.candidate_urls(&crate::host::local_addrs());
        if urls.len() > 1 {
            urls
        } else {
            Vec::new()
        }
    }

    /// The login in flight sent a saved device token / used an invite with these addresses.
    pub(crate) fn servers_login_started(&mut self, device_used: bool, invite: Option<(u16, &[InviteAddr])>) {
        self.servers.device_used = device_used;
        self.servers.pending_invite = invite.map(|(port, addrs)| (port, addrs.iter().map(AltAddr::from).collect()));
    }

    /// A login went through: save or update its entry ("Salvar na lista de servidores").
    /// `server_id`: the server's identity, when the login confirmed it (v1.0.1). `tls_pin`: its
    /// certificate, when the entry may keep it (v1.1).
    pub(crate) fn servers_logged_in(&mut self, url: &str, username: &str, server_name: &str, personal_typed: bool, server_id: Option<&str>, tls_pin: Option<[u8; 32]>) {
        let entry = self.servers.joining.take();
        let invite = self.servers.pending_invite.take();
        let device_used = std::mem::take(&mut self.servers.device_used);
        self.servers.edit = None;
        self.servers.pinger.stop();
        if !self.form.save_server {
            tracing::info!("logged in; not saved in the server list (switched off)");
            return;
        }
        let hosted = self.hosted_login(url);
        let login = Login {
            url,
            username,
            server_name,
            password: &self.form.password,
            // My own server's password comes from its folder.
            remember: self.form.remember && hosted.is_none(),
            personal_typed,
            device_used,
            entry,
            invite,
            hosted,
            server_id,
            tls_pin,
            now_ms: crate::app::now_ms(),
        };
        let id = self.settings.remember_server(login);
        tracing::info!(id, %url, entries = self.settings.servers.servers.len(), "server list: entry saved");
    }

    /// A login failed: the form shows (filled in) with the reason.
    pub(crate) fn servers_login_failed(&mut self) {
        self.servers.form_open = true;
    }

    /// "+ Adicionar servidor": the form, for a server not in the list yet (the name stays).
    pub fn servers_add_new(&mut self) {
        self.servers.form_open = true;
        self.servers.joining = None;
        self.servers.edit = None;
        self.form.url.clear();
        self.form.password.clear();
        self.form.personal.clear();
        self.form.need_personal = false;
        self.form.error = None;
        self.form.manual = None;
    }

    /// "Remover da lista" (confirmed): the entry, its saved password and its login token go.
    pub fn servers_remove(&mut self, id: u64) {
        if self.settings.remove_server(id) {
            tracing::info!(id, "server list: entry removed with its secrets");
            if self.servers.joining == Some(id) {
                self.servers.joining = None;
            }
            self.servers.pinger.online.remove(&id);
            self.save_settings();
        }
        if self.settings.servers.servers.is_empty() {
            self.servers.form_open = true;
        }
    }

    /// "Esquecer senha".
    pub fn servers_forget_password(&mut self, id: u64) {
        self.settings.forget_server_password(id);
        tracing::info!(id, "server list: saved password forgotten");
        self.save_settings();
    }

    /// "Trocar de servidor": leave the call (voice, camera, screen share), log out and go back to
    /// the list. A server I host keeps running (only leaving the app stops it).
    pub fn switch_server(&mut self) {
        tracing::info!(hosting = self.host.hosting(), "switching server");
        if self.settings_ui.mic_test != MicTestUi::Off {
            self.stop_mic_test();
        }
        if self.settings_ui.camera_preview != CameraPreviewUi::Off {
            self.settings_ui.camera_preview = CameraPreviewUi::Off;
            self.backend.send(Cmd::CameraPreview(None));
        }
        if self.voice.channel.is_some() || self.voice.joining.is_some() {
            self.leave_voice();
        }
        self.logout();
        self.host.tab = HomeTab::Connect;
        self.servers.joining = None;
        self.servers.edit = None;
        self.servers.form_open = self.settings.servers.servers.is_empty();
        self.servers.pinger.soon();
        self.form.error = None;
    }

    // ------------------------------------------------------------ UI

    /// "Seus servidores salvos" above the form; returns whether the form shows under it.
    pub(crate) fn saved_servers(&mut self, ui: &mut egui::Ui) -> bool {
        if self.settings.servers.servers.is_empty() {
            self.servers.form_open = true;
            return true;
        }
        let mut act = None;
        // (I6: narrower in a small or zoomed-in window: the rows' buttons stay in it.)
        let list_w = LIST_W.min(ui.available_width());
        ui.allocate_ui_with_layout(egui::vec2(list_w, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_width(list_w);
            crate::ui_kit::title(ui, "Seus servidores");
            ui.add_space(10.0);
            let list = self.settings.servers.servers.clone();
            let n = list.len();
            for (i, e) in list.iter().enumerate() {
                self.server_row(ui, e, i, n, &mut act);
                self.server_editor(ui, e, &mut act);
                ui.add_space(2.0);
            }
            ui.add_space(10.0);
            if self.servers.form_open {
                ui.horizontal(|ui| {
                    crate::ui_kit::title(ui, "Adicionar servidor");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if crate::ui_kit::button_sized(ui, crate::ui_kit::Kind::Ghost, crate::ui_kit::Size::Small, None, "Fechar").clicked() {
                            act = Some(Act::CloseForm);
                        }
                    });
                });
                ui.add_space(8.0);
            } else if crate::ui_kit::button(ui, crate::ui_kit::Kind::Ghost, Some(crate::theme::ph::PLUS), "Adicionar servidor")
                .on_hover_text("Entrar em outro servidor (endereço ou código de convite)")
                .clicked()
            {
                act = Some(Act::Add);
            }
        });
        if let Some(a) = act {
            self.server_act(a);
        }
        self.servers.form_open
    }

    fn server_row(&mut self, ui: &mut egui::Ui, e: &SavedServer, i: usize, n: usize, act: &mut Option<Act>) {
        // Logging in, or (my own server) getting it ready first.
        let joining_here = self.servers.joining == Some(e.id) && (self.form.busy || (e.hosted.is_some() && self.host.hosting()));
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(LIST_W.min(ui.available_width()), ROW_H), Sense::click());
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        let theme_fill = if resp.hovered() || joining_here { crate::theme::MOVEL } else { Color32::TRANSPARENT };
        ui.painter().rect_filled(rect, crate::theme::radius::BUTTON, theme_fill);
        // The badge: the name's first letter on a rounded square (hubs have no icon of their own).
        let title = e.title();
        let badge = egui::Rect::from_center_size(egui::pos2(rect.left() + 10.0 + 20.0, rect.center().y), egui::vec2(40.0, 40.0));
        ui.painter().rect_filled(badge, crate::theme::radius::CARD, badge_color(&title));
        let initial = title.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".into());
        ui.painter().text(badge.center(), egui::Align2::CENTER_CENTER, initial, crate::theme::display(18.0), crate::theme::NOITE);

        let inner = egui::Rect::from_min_max(egui::pos2(rect.left() + 62.0, rect.top() + 4.0), egui::pos2(rect.right() - 8.0, rect.bottom() - 4.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(Layout::right_to_left(Align::Center)));
        child.spacing_mut().item_spacing.x = 6.0;
        let more = crate::ui_kit::icon_button(&mut child, crate::theme::ph::DOTS_THREE, "Mais opções (o mesmo menu do botão direito)", crate::ui_kit::IconState::Normal);
        egui::Popup::menu(&more).frame(crate::ui_kit::popup_frame(child.style())).show(|ui| Self::server_menu(ui, e, i, n, act));
        if joining_here {
            child.label(RichText::new("Conectando...").font(crate::theme::body(12.0)).color(crate::theme::NEVOA));
            child.spinner();
        }
        let (color, text, hover) = match self.servers.pinger.online.get(&e.id) {
            Some(true) => (GREEN, "online", "O servidor respondeu agora há pouco."),
            Some(false) if e.hosted.is_some() => (OFFLINE_GRAY, "desligado", "Seu servidor está desligado: clique para ligá-lo e entrar."),
            Some(false) => (OFFLINE_GRAY, "offline", "O servidor não respondeu (desligado, ou fora do alcance desta rede)."),
            None => (OFFLINE_GRAY.gamma_multiply(0.5), "...", "Verificando..."),
        };
        let (dot, _) = child.allocate_exact_size(egui::vec2(12.0, 12.0), Sense::hover());
        child.painter().circle_filled(dot.center(), 4.5, color);
        child.interact(dot.expand(4.0), child.id().with(("srv_state", e.id)), Sense::hover()).on_hover_text(format!("{text}: {hover}"));
        child.with_layout(Layout::top_down(Align::Min), |ui| {
            ui.add_space(6.0);
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add(egui::Label::new(RichText::new(&title).font(crate::theme::semibold(14.5)).color(crate::theme::GIZ)).truncate().selectable(false));
            let mut sub = Vec::new();
            if !e.nickname.trim().is_empty() {
                sub.push(if e.server_name.trim().is_empty() { shown_address(&e.url) } else { e.server_name.clone() });
            }
            sub.push(e.username.clone());
            let last = last_used_text(e.last_used_ms, crate::app::now_ms(), crate::app::local_offset_min());
            if !last.is_empty() {
                sub.push(last);
            }
            if e.hosted.is_some() {
                sub.push("seu servidor".into());
            }
            ui.add(egui::Label::new(RichText::new(sub.join(" · ")).font(crate::theme::body(12.0)).color(crate::theme::NEVOA)).truncate().selectable(false));
        });
        let hover = format!("{}\nClique para entrar · botão direito: mais opções", shown_address(&e.url));
        let resp = resp.on_hover_text(hover);
        if resp.clicked() && !self.form.busy {
            *act = Some(Act::Join(e.id));
        }
        egui::Popup::context_menu(&resp).at_pointer_fixed().frame(crate::ui_kit::popup_frame(ui.style())).style(crate::ui_kit::menu_style).show(|ui| Self::server_menu(ui, e, i, n, act));
    }

    fn server_menu(ui: &mut egui::Ui, e: &SavedServer, i: usize, n: usize, act: &mut Option<Act>) {
        use crate::theme::ph;
        ui.set_min_width(190.0);
        let mut item = |ui: &mut egui::Ui, enabled: bool, icon: &str, text: &str, danger: bool, a: Act| {
            if ui.add_enabled_ui(enabled, |ui| crate::ui_kit::menu_row(ui, icon, text, danger)).inner.clicked() {
                *act = Some(a);
                ui.close();
            }
        };
        item(ui, true, ph::ARROW_RIGHT, "Entrar", false, Act::Join(e.id));
        crate::ui_kit::menu_separator(ui);
        item(ui, true, ph::PENCIL_SIMPLE, "Renomear...", false, Act::Rename(e.id));
        item(ui, true, ph::HARD_DRIVES, "Editar endereço...", false, Act::EditAddress(e.id));
        item(ui, e.password_enc.is_some(), ph::LOCK_SIMPLE, "Esquecer senha", false, Act::ForgetPassword(e.id));
        crate::ui_kit::menu_separator(ui);
        item(ui, i > 0, ph::CARET_UP, "Mover para cima", false, Act::Move(e.id, true));
        item(ui, i + 1 < n, ph::CARET_DOWN, "Mover para baixo", false, Act::Move(e.id, false));
        crate::ui_kit::menu_separator(ui);
        item(ui, true, ph::TRASH, "Remover da lista...", true, Act::AskRemove(e.id));
    }

    /// The inline editor under a row ("Renomear", "Editar endereço"), and the removal question
    /// (the kit's dialog, "Remover" on the right).
    fn server_editor(&mut self, ui: &mut egui::Ui, e: &SavedServer, act: &mut Option<Act>) {
        use crate::ui_kit::{self, Kind, Size};
        let Some(edit) = self.servers.edit.as_mut() else { return };
        let editor = |ui: &mut egui::Ui, text: &mut String, hint: &str, act: &mut Option<Act>| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let r = ui.add(ui_kit::field(text).hint_text(hint).desired_width((LIST_W.min(ui.available_width()) - 150.0).max(80.0)));
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui_kit::button_sized(ui, Kind::Primary, Size::Small, None, "Salvar").clicked() || enter {
                    *act = Some(Act::SaveEdit);
                }
                if ui_kit::button_sized(ui, Kind::Ghost, Size::Small, None, "Cancelar").clicked() {
                    *act = Some(Act::CancelEdit);
                }
            });
        };
        match edit {
            Edit::Rename(id, text) if *id == e.id => {
                editor(ui, text, "apelido (vazio: o nome do servidor)", act);
                if !e.server_name.is_empty() {
                    ui_kit::hint(ui, format!("O nome do servidor (\"{}\") continua aparecendo embaixo do apelido.", e.server_name));
                }
            }
            Edit::Address(id, text, error) if *id == e.id => {
                editor(ui, text, "192.168.0.10 ou meu.servidor.com:7890", act);
                if let Some(err) = error {
                    ui_kit::result_text(ui, false, err.as_str());
                }
            }
            Edit::Remove(id) if *id == e.id => {
                let answer = ui_kit::confirm(ui.ctx(), "server_remove", "Remover servidor", "Remover", Kind::Danger, true, |ui| {
                    ui_kit::body_text(ui, format!("Remover \"{}\" da lista?", e.title()));
                    ui_kit::hint(ui, "A senha salva e o login lembrado deste computador para ele também são apagados. O servidor em si continua existindo.");
                });
                match answer {
                    Some(true) => *act = Some(Act::Remove(e.id)),
                    Some(false) => *act = Some(Act::CancelEdit),
                    None => {}
                }
            }
            _ => {}
        }
    }

    fn server_act(&mut self, a: Act) {
        match a {
            Act::Join(id) => self.join_saved(id),
            Act::Add => self.servers_add_new(),
            Act::CloseForm => {
                self.servers.form_open = false;
                self.form.error = None;
            }
            Act::Rename(id) => {
                let nick = self.settings.server_entry(id).map(|e| e.nickname.clone()).unwrap_or_default();
                self.servers.edit = Some(Edit::Rename(id, nick));
            }
            Act::EditAddress(id) => {
                let url = self.settings.server_entry(id).map(|e| shown_address(&e.url)).unwrap_or_default();
                self.servers.edit = Some(Edit::Address(id, url, None));
            }
            Act::ForgetPassword(id) => self.servers_forget_password(id),
            Act::AskRemove(id) => self.servers.edit = Some(Edit::Remove(id)),
            Act::Move(id, up) => {
                self.settings.move_server(id, up);
                self.save_settings();
            }
            Act::SaveEdit => match self.servers.edit.take() {
                Some(Edit::Rename(id, text)) => {
                    self.settings.rename_server(id, &text);
                    self.save_settings();
                }
                Some(Edit::Address(id, text, _)) => match self.settings.edit_server_address(id, &text) {
                    Ok(()) => {
                        tracing::info!(id, "server list: address edited");
                        self.servers.pinger.online.remove(&id);
                        self.servers.pinger.soon();
                        self.save_settings();
                    }
                    Err(err) => self.servers.edit = Some(Edit::Address(id, text, Some(err))),
                },
                other => self.servers.edit = other,
            },
            Act::Remove(id) => {
                self.servers.edit = None;
                self.servers_remove(id);
            }
            Act::CancelEdit => self.servers.edit = None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login<'a>(url: &'a str, user: &'a str, password: &'a str, remember: bool) -> Login<'a> {
        Login {
            url,
            username: user,
            server_name: "Gate",
            password,
            remember,
            personal_typed: false,
            device_used: false,
            entry: None,
            invite: None,
            hosted: None,
            server_id: None,
            tls_pin: None,
            now_ms: 1_000,
        }
    }

    /// v1.1 (T0): the certificate pin stays with the entry and its token, and moves with them.
    #[test]
    fn the_tls_pin_stays_with_the_entry_and_its_token() {
        let mut s = Settings::default();
        let (p, q) = ([3u8; 32], [4u8; 32]);
        let a = s.remember_server(login("192.0.2.10", "ana", "", false));
        assert_eq!(s.server_entry(a).unwrap().tls_pin(), None, "a plain login: none");
        s.remember_server(Login { entry: Some(a), tls_pin: Some(p), ..login("192.0.2.10", "ana", "", false) });
        assert_eq!(s.server_entry(a).unwrap().tls_pin(), Some(p));
        s.remember_server(Login { entry: Some(a), ..login("192.0.2.10", "ana", "", false) });
        assert_eq!(s.server_entry(a).unwrap().tls_pin(), Some(p), "never forgotten by a login without one");
        s.set_device_token("http://192.0.2.10:7890", "ana", Some("tok"));
        s.set_device_token_pin("http://192.0.2.10:7890", "ana", Some(p));
        assert_eq!(s.device_token_pin("http://192.0.2.10:7890", "Ana"), Some(p));
        // The entry moves to another address: its token and the token's pin go along.
        s.remember_server(Login { entry: Some(a), tls_pin: Some(p), ..login("192.0.2.11", "ana", "", false) });
        assert_eq!(s.device_token_pin("http://192.0.2.11:7890", "ana"), Some(p));
        assert_eq!(s.device_token_pin("http://192.0.2.10:7890", "ana"), None);
        // "Confiar no novo código".
        s.trust_new_pin("192.0.2.11", "ana", q);
        assert_eq!(s.server_entry(a).unwrap().tls_pin(), Some(q));
        assert_eq!(s.device_token_pin("http://192.0.2.11:7890", "ana"), Some(q));
        // A new token starts without a pin; the pin is not a secret but it round-trips.
        s.set_device_token("http://192.0.2.11:7890", "ana", Some("tok2"));
        assert_eq!(s.device_token_pin("http://192.0.2.11:7890", "ana"), None);
        let json = serde_json::to_string(s.server_entry(a).unwrap()).unwrap();
        assert!(json.contains(&vox_proto::tls_pin_hex(&q)), "{json}");
        let plain = serde_json::to_string(&SavedServer::default()).unwrap();
        assert!(!plain.contains("tls_pin"), "absent while unknown: {plain}");
        // Audit (T0): "Editar endereço" moves the token WITH its pin (it used to lose it).
        s.set_device_token_pin("http://192.0.2.11:7890", "ana", Some(q));
        s.edit_server_address(a, "192.0.2.12").unwrap();
        assert_eq!(s.device_token("http://192.0.2.12:7890", "ana").as_deref(), Some("tok2"));
        assert_eq!(s.device_token_pin("http://192.0.2.12:7890", "ana"), Some(q), "the pin follows the token");
        assert_eq!(s.server_entry(a).unwrap().tls_pin(), Some(q), "and the entry keeps its own");
        // A saved settings file of v1.0.2 (no pins anywhere) still loads.
        let old: Settings = serde_json::from_str(r#"{"device_tokens":{},"device_token_servers":{}}"#).unwrap();
        assert!(old.device_token_pins.is_empty());
        let old: SavedServer = serde_json::from_str(r#"{"id":1,"url":"http://192.0.2.10:7890","username":"ana","server_id":"0123456789abcdef0123456789abcdef"}"#).unwrap();
        assert_eq!(old.tls_pin(), None);
    }

    #[test]
    fn addresses_and_names_are_normalized_for_the_dedup() {
        assert_eq!(server_key("192.0.2.10", "Ana"), server_key(" HTTP://192.0.2.10:7890/ ", "ana "));
        assert_ne!(server_key("192.0.2.10", "ana"), server_key("192.0.2.10:7891", "ana"));
        assert_ne!(server_key("192.0.2.10", "ana"), server_key("192.0.2.10", "bia"));
        assert_ne!(server_key("192.0.2.10", "ana"), server_key("https://192.0.2.10", "ana"));
        let mut s = Settings::default();
        let a = s.remember_server(login("192.0.2.10", "Ana", "", false));
        let b = s.remember_server(Login { now_ms: 2_000, ..login("http://192.0.2.10:7890/", "ana", "", false) });
        assert_eq!(a, b, "same server and name: one entry");
        assert_eq!(s.servers.servers.len(), 1);
        assert_eq!(s.servers.servers[0].url, "http://192.0.2.10:7890");
        assert_eq!(s.servers.servers[0].last_used_ms, 2_000);
        let c = s.remember_server(login("192.0.2.10", "Bia", "", false));
        let d = s.remember_server(login("192.0.2.10:7990", "Ana", "", false));
        assert_eq!(s.servers.servers.len(), 3);
        assert!(a != c && c != d && a != d);
        // Ids are never reused after a removal.
        s.remove_server(d);
        let e = s.remember_server(login("192.0.2.11", "Ana", "", false));
        assert!(e > d);
    }

    #[test]
    fn passwords_are_encrypted_per_entry_and_only_when_remembered() {
        let mut s = Settings::default();
        let a = s.remember_server(login("192.0.2.10", "ana", "senha-do-um", true));
        let b = s.remember_server(login("192.0.2.20", "ana", "senha-do-dois", false));
        let ea = s.server_entry(a).unwrap().clone();
        let eb = s.server_entry(b).unwrap().clone();
        assert_eq!(s.server_password(&ea).as_deref(), Some("senha-do-um"));
        assert_eq!(s.server_password(&eb), None);
        assert!(ea.needs_password && eb.needs_password);
        let file = s.servers.to_json();
        assert!(!file.contains("senha-do-um") && !file.contains("senha-do-dois"), "{file}");
        // Unticking "Lembrar a senha" on the next login drops it.
        s.remember_server(login("192.0.2.10", "ana", "senha-do-um", false));
        assert_eq!(s.server_password(s.server_entry(a).unwrap()), None);
    }

    #[test]
    fn the_old_single_server_fields_follow_the_last_used_entry() {
        let mut s = Settings::default();
        let a = s.remember_server(Login { now_ms: 10, ..login("192.0.2.10", "ana", "um", true) });
        s.remember_server(Login { now_ms: 20, ..login("192.0.2.20", "bia", "", false) });
        // What v0.9.0 reads from settings.json.
        #[derive(serde::Deserialize)]
        struct V090 {
            server: String,
            username: String,
            remember_password: bool,
            password_enc: Option<String>,
        }
        let old: V090 = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!((old.server.as_str(), old.username.as_str(), old.remember_password, old.password_enc.is_some()), ("http://192.0.2.20:7890", "bia", false, false));
        // Back to the first one: v0.9.0 finds it with its password (same DPAPI blob).
        s.remember_server(Login { now_ms: 30, entry: Some(a), ..login("192.0.2.10", "ana", "um", true) });
        let old: V090 = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!((old.server.as_str(), old.username.as_str(), old.remember_password), ("http://192.0.2.10:7890", "ana", true));
        assert_eq!(unprotect(old.password_enc.as_deref().unwrap()).as_deref(), Some("um"));
        assert_eq!(s.saved_password().as_deref(), Some("um"));
        // The list in servers.json is not part of settings.json.
        assert!(!serde_json::to_string(&s).unwrap().contains("\"servers\""));
        // Forgetting the password of the latest entry clears the old fields' copy too.
        s.forget_server_password(a);
        assert!(!s.remember_password && s.password_enc.is_none() && s.server == "http://192.0.2.10:7890");
        // An empty list leaves nothing behind.
        for id in s.servers.servers.iter().map(|e| e.id).collect::<Vec<_>>() {
            s.remove_server(id);
        }
        assert!(s.server.is_empty() && s.username.is_empty() && s.password_enc.is_none());
    }

    #[test]
    fn the_v090_single_server_becomes_the_first_entry() {
        let dir = std::env::temp_dir().join(format!("concord-servers-migrate-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("settings.json");
        let enc = protect("senha-antiga").unwrap();
        let tok = protect("token-do-pc").unwrap();
        let v090 = format!(
            r#"{{"server":"http://192.0.2.30:7890","username":"Ana","remember_password":true,"password_enc":"{enc}","volume_pct":80,"device_tokens":{{"http://192.0.2.30:7890|ana":"{tok}"}}}}"#
        );
        std::fs::write(&p, v090).unwrap();
        let s = Settings::load_at(&p);
        assert_eq!(s.volume_pct, 80, "the rest of the file still loads");
        assert_eq!(s.servers.servers.len(), 1);
        let e = s.servers.servers[0].clone();
        assert_eq!((e.url.as_str(), e.username.as_str(), e.last_used_ms), ("http://192.0.2.30:7890", "Ana", 0));
        assert_eq!(s.server_password(&e).as_deref(), Some("senha-antiga"));
        assert_eq!(s.server_device_token(&e).as_deref(), Some("token-do-pc"));
        assert_eq!(plan(&e, true, true, false), Plan::Join);
        // Saved and loaded again: still one entry, and the old fields are still there for v0.9.0.
        s.save_at(&p);
        let raw = std::fs::read_to_string(dir.join("servers.json")).unwrap();
        assert!(!raw.contains("senha-antiga") && !raw.contains("token-do-pc"));
        let back = Settings::load_at(&p);
        assert_eq!(back.servers.servers.len(), 1);
        assert_eq!((back.server.as_str(), back.username.as_str()), ("http://192.0.2.30:7890", "Ana"));
        assert_eq!(back.saved_password().as_deref(), Some("senha-antiga"));
        // After a downgrade, v0.9.0 logged in somewhere else and rewrote settings.json: the list
        // file is untouched and the new server joins it as the newest entry.
        let mut v090 = back.clone();
        v090.server = "192.0.2.40".into();
        v090.username = "ana".into();
        v090.remember_password = false;
        v090.password_enc = None;
        std::fs::write(&p, serde_json::to_string(&v090).unwrap()).unwrap();
        let up = Settings::load_at(&p);
        assert_eq!(up.servers.servers.len(), 2);
        assert_eq!(up.servers.servers[0].url, "http://192.0.2.40:7890");
        assert!(up.servers.servers[0].last_used_ms > 0);
        // An empty list file stays empty (nothing to merge once the old fields are cleared).
        let mut none = up.clone();
        for id in none.servers.servers.iter().map(|e| e.id).collect::<Vec<_>>() {
            none.remove_server(id);
        }
        none.save_at(&p);
        assert!(Settings::load_at(&p).servers.servers.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn removing_an_entry_deletes_its_secrets() {
        let mut s = Settings::default();
        let a = s.remember_server(login("192.0.2.10", "ana", "segredo-a", true));
        let b = s.remember_server(Login { now_ms: 5, ..login("192.0.2.20", "ana", "segredo-b", true) });
        s.set_device_token("http://192.0.2.10:7890", "ana", Some("tok-a"));
        s.set_device_token("http://192.0.2.20:7890", "ana", Some("tok-b"));
        let enc_a = s.server_entry(a).unwrap().password_enc.clone().unwrap();
        assert!(s.remove_server(a));
        assert!(s.server_entry(a).is_none());
        assert_eq!(s.device_token("http://192.0.2.10:7890", "ana"), None);
        assert_eq!(s.device_token("http://192.0.2.20:7890", "ana").as_deref(), Some("tok-b"), "the other entry keeps its own");
        let all = format!("{}{}", serde_json::to_string(&s).unwrap(), s.servers.to_json());
        assert!(!all.contains(&enc_a) && !all.contains("192.0.2.10"), "{all}");
        assert!(s.server_entry(b).is_some());
        assert!(!s.remove_server(a), "already gone");
    }

    #[test]
    fn the_device_token_moves_with_the_entry() {
        let mut s = Settings::default();
        let a = s.remember_server(login("192.0.2.10", "ana", "", false));
        s.set_device_token("http://192.0.2.10:7890", "ana", Some("tok"));
        // One click on the entry, and the hub answered at its other (public) address.
        let b = s.remember_server(Login { entry: Some(a), device_used: true, ..login("203.0.113.9:7890", "ana", "", false) });
        assert_eq!(a, b);
        assert_eq!(s.servers.servers.len(), 1);
        assert_eq!(s.device_token("http://203.0.113.9:7890", "ana").as_deref(), Some("tok"));
        assert_eq!(s.device_token("http://192.0.2.10:7890", "ana"), None);
        // "Editar endereço" too.
        s.edit_server_address(a, "192.0.2.77:7891").unwrap();
        assert_eq!(s.server_device_token(s.server_entry(a).unwrap()).as_deref(), Some("tok"));
        assert_eq!(s.device_token("http://192.0.2.77:7891", "ana").as_deref(), Some("tok"));
        assert!(s.edit_server_address(a, "vox1-ABCDEF").is_err());
        assert!(s.edit_server_address(a, "  ").is_err());
        let other = s.remember_server(login("192.0.2.5", "ana", "", false));
        assert!(s.edit_server_address(other, "http://192.0.2.77:7891").is_err(), "would duplicate an entry");
    }

    /// v1.0.1: the server's identity is kept with the entry and with its login token, follows
    /// them when the address changes, and removing one entry leaves another entry's token alone.
    #[test]
    fn the_server_identity_stays_with_the_entry_and_its_token() {
        let id = "0123456789abcdef0123456789abcdef";
        let mut s = Settings::default();
        let a = s.remember_server(login("192.0.2.10", "ana", "", false));
        assert_eq!(s.server_entry(a).unwrap().server_id(), None, "not confirmed: none");
        s.remember_server(Login { entry: Some(a), server_id: Some(id), ..login("192.0.2.10", "ana", "", false) });
        assert_eq!(s.server_entry(a).unwrap().server_id().as_deref(), Some(id));
        // A later login that did not confirm it leaves it as it was.
        s.remember_server(Login { entry: Some(a), ..login("192.0.2.10", "ana", "", false) });
        assert_eq!(s.server_entry(a).unwrap().server_id().as_deref(), Some(id));
        let json = s.servers.to_json();
        assert_eq!(ServerList::from_json(&json).unwrap().servers, s.servers.servers);
        assert!(json.contains(id));

        s.set_device_token("http://192.0.2.10:7890", "ana", Some("tok"));
        s.set_device_token_server("http://192.0.2.10:7890", "ana", Some(id));
        assert_eq!(s.device_token_server("HTTP://192.0.2.10:7890", "Ana").as_deref(), Some(id));
        s.set_device_token_server("http://192.0.2.99:7890", "ana", Some(id));
        assert_eq!(s.device_token_server("http://192.0.2.99:7890", "ana"), None, "no token there: nothing to tie");
        s.edit_server_address(a, "192.0.2.77").unwrap();
        assert_eq!(s.device_token_server("http://192.0.2.77:7890", "ana").as_deref(), Some(id), "follows the token");
        assert_eq!(s.device_token_server("http://192.0.2.10:7890", "ana"), None);
        // A new token starts without one until the app ties it.
        s.set_device_token("http://192.0.2.77:7890", "ana", Some("tok2"));
        assert_eq!(s.device_token_server("http://192.0.2.77:7890", "ana"), None);

        // An entry from an invite lists an address where another entry of the same name lives:
        // removing the first keeps the other's token.
        let other = s.remember_server(login("203.0.113.7:7990", "ana", "", false));
        s.set_device_token("http://203.0.113.7:7990", "ana", Some("tok-outro"));
        let addrs = vec![AltAddr::Public { ip: "203.0.113.7".parse().unwrap() }];
        let inv = s.remember_server(Login { invite: Some((7990, addrs)), ..login("198.51.100.4:7990", "ana", "", false) });
        s.set_device_token("http://198.51.100.4:7990", "ana", Some("tok-convite"));
        assert!(s.remove_server(inv));
        assert_eq!(s.device_token("http://203.0.113.7:7990", "ana").as_deref(), Some("tok-outro"));
        assert_eq!(s.device_token("http://198.51.100.4:7990", "ana"), None);
        assert!(s.server_entry(other).is_some());
    }

    #[test]
    fn an_invite_keeps_its_addresses_but_never_the_token() {
        let mut s = Settings::default();
        let addrs = vec![AltAddr::Lan { ip: "192.168.0.20".parse().unwrap(), prefix: 24 }, AltAddr::Public { ip: "203.0.113.7".parse().unwrap() }];
        let id = s.remember_server(Login { invite: Some((7990, addrs)), ..login("http://203.0.113.7:7990", "ana", "", false) });
        let e = s.server_entry(id).unwrap().clone();
        let home: Vec<(IpAddr, u8)> = vec![("192.168.0.5".parse().unwrap(), 24)];
        assert_eq!(e.candidate_urls(&home), ["http://192.168.0.20:7990", "http://203.0.113.7:7990"]);
        assert_eq!(e.candidate_urls(&[]), ["http://203.0.113.7:7990"]);
        let json = s.servers.to_json();
        assert!(json.contains("192.168.0.20") && !json.contains("vox1-") && !json.contains("token"), "{json}");
        let back = ServerList::from_json(&json).unwrap();
        assert_eq!(back.servers, s.servers.servers);
        // Without an invite: just the address.
        let plain = SavedServer { url: "http://192.0.2.1:7890".into(), ..Default::default() };
        assert_eq!(plain.candidate_urls(&home), ["http://192.0.2.1:7890"]);
    }

    #[test]
    fn a_missing_credential_opens_the_form_asking_only_for_it() {
        let e = SavedServer { url: "http://192.0.2.1:7890".into(), username: "ana".into(), ..Default::default() };
        // An open server.
        assert_eq!(plan(&e, false, false, false), Plan::Join);
        // A server password was needed: saved -> join; not saved -> ask for it (a login token
        // proves the name, not the server password).
        let pw = SavedServer { needs_password: true, ..e.clone() };
        assert_eq!(plan(&pw, true, false, false), Plan::Join);
        assert_eq!(plan(&pw, false, false, false), Plan::AskPassword);
        assert_eq!(plan(&pw, false, true, false), Plan::AskPassword);
        // Came with an invite (no server password ever typed): the login token is enough.
        let invited = SavedServer { personal: true, ..e.clone() };
        assert_eq!(plan(&invited, false, true, false), Plan::Join);
        // A protected name without a remembered computer: ask for the personal password.
        let personal = SavedServer { personal: true, ..pw.clone() };
        assert_eq!(plan(&personal, true, false, false), Plan::AskPersonal);
        assert_eq!(plan(&personal, false, false, false), Plan::AskPassword, "the server password first");
        assert_eq!(plan(&personal, true, true, false), Plan::Join);
        // My own hosted server opens it, when its folder is still there.
        let hosted = SavedServer { hosted: Some("abc".into()), needs_password: true, ..e.clone() };
        assert_eq!(plan(&hosted, false, false, true), Plan::Host);
        assert_eq!(plan(&hosted, false, false, false), Plan::AskPassword);
    }

    #[test]
    fn the_list_is_capped_and_the_oldest_goes_with_its_secrets() {
        let mut s = Settings::default();
        for i in 0..MAX_SERVERS {
            s.remember_server(Login { now_ms: 100 + i as i64, ..login(&format!("192.0.2.{}", i + 1), "ana", "", false) });
        }
        s.set_device_token("http://192.0.2.1:7890", "ana", Some("tok-velho"));
        assert_eq!(s.servers.servers.len(), MAX_SERVERS);
        s.remember_server(Login { now_ms: 10_000, ..login("203.0.113.1", "ana", "", false) });
        assert_eq!(s.servers.servers.len(), MAX_SERVERS);
        assert!(!s.servers.servers.iter().any(|e| e.url == "http://192.0.2.1:7890"), "the least recently used went");
        assert_eq!(s.device_token("http://192.0.2.1:7890", "ana"), None);
    }

    #[test]
    fn the_file_loads_leniently() {
        let text = r#"{"version":1,"next_id":3,"servers":[
            {"id":1,"url":"http://192.0.2.1:7890","username":"ana","server_name":"Um"},
            {"id":"bad","url":7},
            {"id":1,"url":"http://192.0.2.2:7890","username":"ana","alt_addrs":[{"lan":{"ip":"192.168.0.2","prefix":24}}],"alt_port":7890},
            {"id":9,"url":"http://192.0.2.1:7890","username":"ANA"},
            {"url":"","username":"x"},
            {"id":5,"url":"http://192.0.2.3:7890","username":"bia","future_field":true}
        ]}"#;
        let l = ServerList::from_json(text).unwrap();
        let urls: Vec<&str> = l.servers.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(urls, ["http://192.0.2.1:7890", "http://192.0.2.2:7890", "http://192.0.2.3:7890"]);
        let ids: HashSet<u64> = l.servers.iter().map(|s| s.id).collect();
        assert_eq!(ids.len(), 3, "a repeated id gets a fresh one");
        assert!(l.next_id > *ids.iter().max().unwrap());
        assert!(ServerList::from_json("[1,2]").is_none());
        assert!(ServerList::from_json("não é json").is_none());
    }

    #[test]
    fn rows_reorder_and_rename() {
        let mut s = Settings::default();
        let a = s.remember_server(login("192.0.2.1", "ana", "", false));
        let b = s.remember_server(login("192.0.2.2", "ana", "", false));
        s.move_server(b, true);
        assert_eq!(s.servers.servers.iter().map(|e| e.id).collect::<Vec<_>>(), [b, a]);
        s.move_server(b, true);
        s.move_server(a, false);
        assert_eq!(s.servers.servers.iter().map(|e| e.id).collect::<Vec<_>>(), [b, a]);
        s.rename_server(a, "  Casa do Zé ");
        assert_eq!(s.server_entry(a).unwrap().title(), "Casa do Zé");
        s.rename_server(a, "");
        assert_eq!(s.server_entry(a).unwrap().title(), "Gate");
        let bare = SavedServer { url: "http://192.0.2.9:7890".into(), ..Default::default() };
        assert_eq!(bare.title(), "192.0.2.9:7890");
    }

    #[test]
    fn last_used_reads_in_portuguese() {
        let day = 86_400_000i64;
        let now = 1_700_000_000_000i64;
        assert_eq!(last_used_text(0, now, 0), "");
        assert_eq!(last_used_text(now - 60_000, now, 0), "agora há pouco");
        assert_eq!(last_used_text(now - day, now, 0), "ontem");
        assert_eq!(last_used_text(now - 3 * day, now, 0), "há 3 dias");
        assert_eq!(last_used_text(now - 8 * day, now, 0), "há 1 semana");
        assert_eq!(last_used_text(now - 15 * day, now, 0), "há 2 semanas");
        assert_eq!(last_used_text(now - 45 * day, now, 0), "há 1 mês");
        assert_eq!(last_used_text(now - 100 * day, now, 0), "há 3 meses");
        assert_eq!(last_used_text(now - 800 * day, now, 0), "há 2 anos");
        // Local midnight decides "hoje" / "ontem" (UTC-3 here).
        let midnight_local = (now / day) * day + 3 * 3_600_000; // 00:00 local
        assert_eq!(last_used_text(midnight_local - 60_000, midnight_local + 2 * 3_600_000, -180), "ontem");
        assert_eq!(last_used_text(midnight_local + 60_000, midnight_local + 2 * 3_600_000, -180), "hoje");
    }

    #[test]
    fn the_ping_waits_its_turn() {
        let mut p = Pinger::default();
        let now = Instant::now();
        assert_eq!(p.due_in(now), Duration::ZERO);
        p.last = Some(now);
        assert!(p.due_in(now) > Duration::from_secs(29));
        p.soon();
        assert_eq!(p.due_in(now), Duration::ZERO);
        // A round against nothing that answers ends quickly and reports offline.
        p.start(vec![(7, vec!["http://127.0.0.1:9".into()], None)], || {});
        let t = Instant::now();
        while !p.poll() && t.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(p.online.get(&7), Some(&false));
        assert!(!p.busy() && p.rounds == 1);
    }
}
