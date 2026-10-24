//! Host mode ("Criar / abrir servidor", v0.8): a server run by the normal app, in-process.
//!
//! - Each hosted server is a folder `<root>/<id>/` (root: `%LOCALAPPDATA%\concord\host`, or
//!   `VOX_HOST_DIR`) holding what `concord-server --data-dir` would: `config.toml` (written here,
//!   with a random server password and LiveKit key / secret), the database, `livekit.yaml`...
//! - The voice server (LiveKit) is downloaded once into `<root>/bin/` ([`livekit_fetch`]).
//! - [`HostRunner`] runs one server on its own thread and tokio runtime, through the same
//!   [`crate::serve_until`] as `concord-server.exe`: it picks free ports ([`ports`]), asks the
//!   router to open them and finds the public address ([`upnp`], [`stun`], [`nat`]), writes the
//!   config and serves until stopped, then removes the port mappings. Nothing of this exists
//!   until somebody hosts: an app that only connects never starts a runtime or a thread here.
//! - The host's own app then logs in through `127.0.0.1`, which makes it the owner ("quem hospeda
//!   vira dono", `owner.rs`): the loopback rule is unchanged, remote people never come from
//!   loopback.

pub mod livekit_fetch;
pub mod nat;
pub mod net;
pub mod ports;
pub mod stun;
pub mod upnp;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::sync::watch;
use vox_proto::InviteAddr;

use crate::util::random_hex;
use nat::Reach;
use ports::PortBlock;
use upnp::{Mapped, PortMapper};

/// Longest server name.
pub const NAME_MAX: usize = 40;
/// How long the router search and the STUN question may take before the server starts anyway.
const NET_TIMEOUT: Duration = Duration::from_secs(4);

/// Where hosted servers live: `VOX_HOST_DIR` (tests), else `%LOCALAPPDATA%\concord\host`.
pub fn root_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("VOX_HOST_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(d).join("concord").join("host");
    }
    // macOS: ~/Library/Application Support/concord/host (the temp dir would lose the server).
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Library").join("Application Support").join("concord").join("host");
    }
    std::env::temp_dir().join("concord").join("host")
}

/// Where the downloaded voice server lives.
pub fn bin_dir(root: &Path) -> PathBuf {
    root.join("bin")
}

/// A server created in host mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hosted {
    pub id: String,
    pub dir: PathBuf,
    pub name: String,
    pub ports: PortBlock,
    /// The server password (for people without an invite; shown to the host).
    pub password: String,
    /// v1.1 (T0): "Aceitar apps antigos sem conexão protegida" (`tls = "auto"`; off is
    /// `"required"`).
    pub accept_old_apps: bool,
}

/// What this module keeps in a hosted server's `config.toml`.
#[derive(Debug, Clone)]
struct HostedConfig {
    name: String,
    password: String,
    bind_ip: Ipv4Addr,
    public_host: String,
    ports: PortBlock,
    api_key: String,
    api_secret: String,
    use_external_ip: bool,
    /// v1.1 (T0): `auto` or `required` (a hosted server always speaks TLS).
    tls: crate::tls::TlsMode,
}

/// A TOML basic string.
fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn write_config(dir: &Path, c: &HostedConfig) -> Result<()> {
    let text = format!(
        "# Servidor hospedado pelo app concord (\"Criar / abrir servidor\"). O app reescreve este\n\
         # arquivo toda vez que abre o servidor (portas e endereço público).\n\
         server_name = {name}\n\
         server_password = {password}\n\
         bind = \"{bind_ip}:{hub}\"\n\
         public_host = {public}\n\
         tls = \"{tls}\"\n\n\
         [livekit]\n\
         api_key = {key}\n\
         api_secret = {secret}\n\
         signal_port = {signal}\n\
         rtc_tcp_port = {tcp}\n\
         rtc_udp_port = {udp}\n\
         use_external_ip = {ext}\n",
        name = toml_str(&c.name),
        password = toml_str(&c.password),
        bind_ip = c.bind_ip,
        hub = c.ports.hub,
        public = toml_str(&c.public_host),
        key = toml_str(&c.api_key),
        secret = toml_str(&c.api_secret),
        signal = c.ports.signal,
        tcp = c.ports.rtc_tcp,
        udp = c.ports.rtc_udp,
        ext = c.use_external_ip,
        tls = if c.tls == crate::tls::TlsMode::Required { "required" } else { "auto" },
    );
    let path = dir.join("config.toml");
    let tmp = dir.join("config.toml.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn read_config(dir: &Path) -> Result<HostedConfig> {
    if !dir.join("config.toml").exists() {
        bail!("{} has no config.toml", dir.display());
    }
    let cfg = crate::config::load_or_create(dir)?;
    let (bind_ip, hub) = cfg.bind.rsplit_once(':').and_then(|(ip, p)| Some((ip.parse().ok()?, p.parse().ok()?))).unwrap_or((Ipv4Addr::UNSPECIFIED, 7890));
    Ok(HostedConfig {
        name: cfg.server_name,
        password: cfg.server_password,
        bind_ip,
        public_host: cfg.public_host,
        ports: PortBlock { hub, signal: cfg.livekit.signal_port, rtc_tcp: cfg.livekit.rtc_tcp_port, rtc_udp: cfg.livekit.rtc_udp_port },
        api_key: cfg.livekit.api_key,
        api_secret: cfg.livekit.api_secret,
        use_external_ip: cfg.livekit.use_external_ip,
        tls: cfg.tls,
    })
}

/// The hosted server in `dir`.
pub fn load(dir: &Path) -> Result<Hosted> {
    let c = read_config(dir)?;
    let id = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(Hosted { id, dir: dir.to_path_buf(), name: c.name, ports: c.ports, password: c.password, accept_old_apps: c.tls != crate::tls::TlsMode::Required })
}

/// v1.1 (T0): "Aceitar apps antigos sem conexão protegida" for the hosted server in `dir`
/// (`tls = "auto"`, or `"required"` when off). Kept in its `config.toml`; a running server also
/// takes it at once ([`HostRunner::set_accept_old_apps`]).
pub fn set_accept_old_apps(dir: &Path, on: bool) -> Result<()> {
    let mut c = read_config(dir)?;
    c.tls = if on { crate::tls::TlsMode::Auto } else { crate::tls::TlsMode::Required };
    write_config(dir, &c)?;
    tracing::info!(dir = %dir.display(), tls = c.tls.as_str(), "host: old apps setting changed");
    Ok(())
}

/// Every hosted server under `root`, oldest first.
pub fn list(root: &Path) -> Vec<Hosted> {
    let Ok(rd) = std::fs::read_dir(root) else { return Vec::new() };
    let mut out: Vec<(std::time::SystemTime, Hosted)> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()) && e.file_name() != "bin")
        .filter_map(|e| {
            let h = load(&e.path()).ok()?;
            let made = e.metadata().and_then(|m| m.created()).unwrap_or(std::time::UNIX_EPOCH);
            Some((made, h))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));
    out.into_iter().map(|(_, h)| h).collect()
}

/// "Criar servidor": a new folder with a fresh config (random password and LiveKit keys; the
/// ports are chosen when it opens).
pub fn create(root: &Path, name: &str) -> Result<Hosted> {
    let name = name.trim();
    let n = name.chars().count();
    if n == 0 || n > NAME_MAX || name.chars().any(char::is_control) {
        bail!("O nome do servidor precisa ter de 1 a {NAME_MAX} caracteres.");
    }
    let id = random_hex::<4>();
    let dir = root.join(&id);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let cfg = HostedConfig {
        name: name.to_string(),
        password: random_hex::<8>(),
        bind_ip: Ipv4Addr::UNSPECIFIED,
        public_host: "127.0.0.1".into(),
        ports: PortBlock::DEFAULT,
        api_key: format!("API{}", random_hex::<8>()),
        api_secret: random_hex::<32>(),
        use_external_ip: false,
        tls: crate::tls::TlsMode::Auto,
    };
    write_config(&dir, &cfg)?;
    tracing::info!(id, name, dir = %dir.display(), "host: server created");
    load(&dir)
}

// ---------------------------------------------------------------- running

/// Whether the network part (router, STUN) is real.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetMode {
    Real,
    /// No UPnP and no STUN at all (tests: `VOX_HOST_OFFLINE=1`): the router is never touched.
    Off,
}

pub struct RunOptions {
    pub net: NetMode,
    /// A router to use instead of searching for one (tests: a fake).
    pub mapper: Option<Arc<dyn PortMapper>>,
    /// Where the hub listens: every interface normally; `127.0.0.1` in tests (`VOX_HOST_BIND`).
    pub bind_ip: Ipv4Addr,
    /// First port block tried (tests start at 1: never the live server's 7890 / 7880-7882).
    pub first_block: u16,
    /// The voice server; `None` runs without voice (tests).
    pub livekit_bin: Option<PathBuf>,
    /// The LAN address to advertise instead of the detected one (`VOX_HOST_LAN=127.0.0.1/8`:
    /// tests, where everything is on this PC).
    pub lan_override: Option<(Ipv4Addr, u8)>,
}

impl RunOptions {
    /// The real thing, with the test switches read from the environment.
    pub fn from_env(livekit_bin: Option<PathBuf>) -> RunOptions {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let offline = env("VOX_HOST_OFFLINE").is_some_and(|v| v != "0");
        let lan_override = env("VOX_HOST_LAN").and_then(|v| {
            let (ip, p) = v.split_once('/').unwrap_or((v.as_str(), "24"));
            Some((ip.trim().parse().ok()?, p.trim().parse().ok()?))
        });
        RunOptions {
            net: if offline { NetMode::Off } else { NetMode::Real },
            mapper: None,
            bind_ip: env("VOX_HOST_BIND").and_then(|v| v.parse().ok()).unwrap_or(Ipv4Addr::UNSPECIFIED),
            first_block: env("VOX_HOST_FIRST_BLOCK").and_then(|v| v.parse().ok()).unwrap_or(0),
            livekit_bin,
            lan_override,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Starting,
    /// Opening the ports on the router, finding the public address.
    Network,
    Running,
    Stopping,
    Stopped,
    Failed(String),
}

/// The router part, for the host panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpnpState {
    /// Not tried (tests).
    Off,
    Searching,
    /// No router answered UPnP (or it refused everything): manual forwarding needed.
    Unavailable(String),
    /// Ports mapped (maybe not all: see [`Mapped::failed`]).
    Mapped(Mapped),
}

/// Everything the host panel shows.
#[derive(Debug, Clone)]
pub struct HostStatus {
    pub phase: Phase,
    pub id: String,
    pub name: String,
    pub password: String,
    pub ports: Option<PortBlock>,
    /// Where the hub really listens.
    pub hub_addr: Option<SocketAddr>,
    pub lan: Option<(Ipv4Addr, u8)>,
    pub upnp: UpnpState,
    pub router_wan: Option<IpAddr>,
    pub stun_ip: Option<Ipv4Addr>,
    pub public_v6: Option<Ipv6Addr>,
    pub reach: Option<Reach>,
    /// v1.1 (T0): the SHA-256 of the server's TLS certificate, once it runs (its "Código do
    /// servidor" is shown to the host).
    pub tls_pin: Option<[u8; 32]>,
    /// v1.1 (T0): "Aceitar apps antigos sem conexão protegida".
    pub accept_old_apps: bool,
}

impl HostStatus {
    fn new(h: &Hosted) -> Self {
        HostStatus {
            phase: Phase::Starting,
            id: h.id.clone(),
            name: h.name.clone(),
            password: h.password.clone(),
            ports: None,
            hub_addr: None,
            lan: None,
            upnp: UpnpState::Searching,
            router_wan: None,
            stun_ip: None,
            public_v6: None,
            reach: None,
            tls_pin: None,
            accept_old_apps: h.accept_old_apps,
        }
    }

    /// The public IPv4 friends on the internet use: the router's when it is a real public one,
    /// else what STUN saw.
    pub fn public_ipv4(&self) -> Option<Ipv4Addr> {
        let wan = match self.router_wan {
            Some(IpAddr::V4(v4)) if !matches!(self.reach, Some(Reach::Cgnat(_))) => Some(v4),
            _ => None,
        };
        wan.or(self.stun_ip)
    }

    /// The addresses an invite carries: the LAN one, the public IPv4, and the public IPv6 when
    /// IPv4 cannot work (CGNAT) or is unknown.
    pub fn invite_addrs(&self) -> Vec<InviteAddr> {
        let mut v = Vec::new();
        if let Some((ip, p)) = self.lan {
            v.push(InviteAddr::Lan(ip, p));
        }
        let v4 = self.public_ipv4();
        if let Some(ip) = v4 {
            v.push(InviteAddr::Public(IpAddr::V4(ip)));
        }
        if v4.is_none() || matches!(self.reach, Some(Reach::Cgnat(_))) {
            if let Some(ip) = self.public_v6 {
                v.push(InviteAddr::Public(IpAddr::V6(ip)));
            }
        }
        v
    }
}

/// A hosted server running on its own thread. Dropping it stops the server (and waits a little).
pub struct HostRunner {
    status: Arc<Mutex<HostStatus>>,
    stop_tx: watch::Sender<bool>,
    thread: Option<std::thread::JoinHandle<()>>,
    dir: PathBuf,
    /// The running hub's TLS policy (v1.1), once it runs.
    tls: Arc<Mutex<Option<crate::tls::Policy>>>,
}

type Wake = Arc<dyn Fn() + Send + Sync>;

impl HostRunner {
    /// Start `hosted` (returns at once; follow [`HostRunner::status`]). `wake` is called whenever
    /// the status changes.
    pub fn start(hosted: &Hosted, opts: RunOptions, wake: Wake) -> HostRunner {
        let status = Arc::new(Mutex::new(HostStatus::new(hosted)));
        let (stop_tx, stop_rx) = watch::channel(false);
        let tls = Arc::new(Mutex::new(None));
        let (st, h, t) = (status.clone(), hosted.clone(), tls.clone());
        let thread = std::thread::Builder::new()
            .name("host".into())
            .spawn(move || run(h, opts, st, stop_rx, wake, t))
            .map_err(|e| tracing::error!("host: cannot start a thread: {e}"))
            .ok();
        if thread.is_none() {
            status.lock().unwrap_or_else(|e| e.into_inner()).phase = Phase::Failed("Não foi possível iniciar o servidor.".into());
        }
        HostRunner { status, stop_tx, thread, dir: hosted.dir.clone(), tls }
    }

    /// v1.1 (T0): "Aceitar apps antigos sem conexão protegida": saved in the server's
    /// `config.toml` and applied at once to new connections of the running server.
    pub fn set_accept_old_apps(&self, on: bool) -> Result<()> {
        set_accept_old_apps(&self.dir, on)?;
        if let Some(p) = self.tls.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            p.set(if on { crate::tls::TlsMode::Auto } else { crate::tls::TlsMode::Required });
        }
        self.status.lock().unwrap_or_else(|e| e.into_inner()).accept_old_apps = on;
        Ok(())
    }

    pub fn status(&self) -> HostStatus {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Still starting or serving (not stopped, not failed).
    pub fn is_live(&self) -> bool {
        !matches!(self.status().phase, Phase::Stopped | Phase::Failed(_))
    }

    /// Ask the server to stop (returns at once).
    pub fn stop(&self) {
        let _ = self.stop_tx.send(true);
    }

    /// Stop and wait for the thread, at most `limit`.
    pub fn stop_and_wait(&mut self, limit: Duration) {
        self.stop();
        let Some(t) = self.thread.take() else { return };
        let until = std::time::Instant::now() + limit;
        while !t.is_finished() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        if t.is_finished() {
            let _ = t.join();
        } else {
            tracing::warn!("host: the server did not stop in time");
        }
    }
}

impl Drop for HostRunner {
    fn drop(&mut self) {
        self.stop_and_wait(Duration::from_secs(8));
    }
}

fn set(status: &Mutex<HostStatus>, wake: &Wake, f: impl FnOnce(&mut HostStatus)) {
    f(&mut status.lock().unwrap_or_else(|e| e.into_inner()));
    wake();
}

fn run(h: Hosted, opts: RunOptions, status: Arc<Mutex<HostStatus>>, stop_rx: watch::Receiver<bool>, wake: Wake, tls: Arc<Mutex<Option<crate::tls::Policy>>>) {
    let rt = match crate::runtime() {
        Ok(rt) => rt,
        Err(e) => {
            set(&status, &wake, |s| s.phase = Phase::Failed(format!("Não foi possível iniciar o servidor: {e}")));
            return;
        }
    };
    let result = rt.block_on(serve(&h, opts, status.clone(), stop_rx, wake.clone(), tls));
    match result {
        Ok(()) => set(&status, &wake, |s| s.phase = Phase::Stopped),
        Err(e) => {
            tracing::error!(id = h.id, "host: server failed: {e:#}");
            set(&status, &wake, |s| s.phase = Phase::Failed(format!("{e:#}")));
        }
    }
    // Leftover tasks (LiveKit's log forwarders...) end with the runtime.
    rt.shutdown_timeout(Duration::from_secs(2));
    tracing::info!(id = h.id, "host: server thread done");
}

/// What the network check found.
struct NetFound {
    mapper: Option<Arc<dyn PortMapper>>,
    local: Option<Ipv4Addr>,
    mapped: Option<Mapped>,
}

async fn serve(h: &Hosted, opts: RunOptions, status: Arc<Mutex<HostStatus>>, stop_rx: watch::Receiver<bool>, wake: Wake, tls: Arc<Mutex<Option<crate::tls::Policy>>>) -> Result<()> {
    let mut cfg = read_config(&h.dir)?;
    let bind_ip = opts.bind_ip;
    let block = ports::choose(Some(cfg.ports), opts.first_block, |b| ports::block_is_free(b, bind_ip))
        .context("Nenhum conjunto de portas livre neste computador (7890 a 8790). Feche outros programas e tente de novo.")?;
    if block != cfg.ports {
        tracing::info!(saved = ?cfg.ports, chosen = ?block, "host: using another port block (the saved one is busy or below the first block allowed)");
    }
    let lan = opts.lan_override.or_else(net::lan_ipv4);
    set(&status, &wake, |s| {
        s.ports = Some(block);
        s.lan = lan;
        s.phase = Phase::Network;
        s.upnp = if opts.net == NetMode::Off && opts.mapper.is_none() { UpnpState::Off } else { UpnpState::Searching };
    });

    let found = network(&opts, block, lan, &status, &wake).await;
    if *stop_rx.borrow() {
        cleanup(found.mapper, found.mapped).await;
        return Ok(());
    }

    let st = status.lock().unwrap_or_else(|e| e.into_inner()).clone();
    cfg.ports = block;
    cfg.bind_ip = bind_ip;
    cfg.public_host = st.public_ipv4().map(|ip| ip.to_string()).or_else(|| lan.map(|(ip, _)| ip.to_string())).unwrap_or_else(|| "127.0.0.1".into());
    // LiveKit finds its public address by STUN itself (and keeps offering the LAN one): only
    // worth it, and only safe to ask, when the internet answered.
    cfg.use_external_ip = opts.net == NetMode::Real && st.public_ipv4().is_some();
    write_config(&h.dir, &cfg)?;

    let hub_opts = crate::Options {
        data_dir: h.dir.clone(),
        no_livekit: opts.livekit_bin.is_none(),
        livekit_bin: opts.livekit_bin.clone().unwrap_or_default(),
        bind_override: None,
    };
    let (st2, wake2) = (status.clone(), wake.clone());
    let NetFound { mapper, local, mapped } = found;
    let (undo_mapper, undo_mapped) = (mapper.clone(), mapped.clone());
    let served = crate::serve_until(hub_opts, move |server| {
        let addr = server.addr;
        tracing::info!(id = %st2.lock().unwrap_or_else(|e| e.into_inner()).id, %addr, "host: server running");
        *tls.lock().unwrap_or_else(|e| e.into_inner()) = Some(server.tls_policy());
        let pin = server.tls_pin();
        set(&st2, &wake2, |s| {
            s.hub_addr = Some(addr);
            s.tls_pin = pin;
            s.phase = Phase::Running;
        });
        keep_mapped(stop_rx, mapper, block, local, mapped, st2, wake2)
    })
    .await;
    if served.is_err() {
        // The hub never ran (a port taken meanwhile...): do not leave the router open.
        cleanup(undo_mapper, undo_mapped).await;
    }
    served
}

/// Router and STUN, side by side, each bounded by [`NET_TIMEOUT`].
async fn network(opts: &RunOptions, block: PortBlock, lan: Option<(Ipv4Addr, u8)>, status: &Arc<Mutex<HostStatus>>, wake: &Wake) -> NetFound {
    let mut found = NetFound { mapper: None, local: lan.map(|(ip, _)| ip), mapped: None };
    let real = opts.net == NetMode::Real;
    let given = opts.mapper.clone();
    let router = async move {
        if let Some(m) = given {
            return Ok(m);
        }
        if !real {
            return Err("off".to_string());
        }
        tokio::task::spawn_blocking(|| upnp::discover(NET_TIMEOUT).map(|m| Arc::new(m) as Arc<dyn PortMapper>))
            .await
            .unwrap_or_else(|e| Err(e.to_string()))
    };
    let stun = async move {
        if real {
            stun::public_ipv4(NET_TIMEOUT).await
        } else {
            None
        }
    };
    let (router, seen) = tokio::join!(router, stun);
    let v6 = if real { net::public_ipv6() } else { None };
    match router {
        Ok(m) => {
            let local = m.local_ip().or(found.local);
            let mm = m.clone();
            let probe = tokio::task::spawn_blocking(move || {
                let wan = mm.external_ip().ok();
                let mapped = local.map(|l| upnp::map_block(mm.as_ref(), &block, l, false));
                (wan, mapped)
            })
            .await;
            let (wan, mapped) = probe.unwrap_or((None, None));
            let reach = nat::classify(wan, seen.map(IpAddr::V4), seen_is_mine(seen));
            tracing::info!(?wan, ?seen, ?reach, mapped = ?mapped.as_ref().map(|m| m.ports.len()), "host: network checked");
            set(status, wake, |s| {
                s.router_wan = wan;
                s.stun_ip = seen;
                s.public_v6 = v6;
                s.reach = Some(reach);
                s.upnp = match &mapped {
                    Some(m) if !m.ports.is_empty() => UpnpState::Mapped(m.clone()),
                    Some(_) => UpnpState::Unavailable("o roteador recusou abrir as portas".into()),
                    None => UpnpState::Unavailable("endereço deste PC na rede não encontrado".into()),
                };
            });
            found.local = local;
            found.mapped = mapped.filter(|m| !m.ports.is_empty());
            found.mapper = Some(m);
        }
        Err(why) => {
            let reach = if real { Some(nat::classify(None, seen.map(IpAddr::V4), seen_is_mine(seen))) } else { None };
            if real {
                tracing::info!(why, ?seen, ?reach, "host: no UPnP router");
            }
            set(status, wake, |s| {
                s.stun_ip = seen;
                s.public_v6 = v6;
                s.reach = reach;
                s.upnp = if real { UpnpState::Unavailable(why) } else { UpnpState::Off };
            });
        }
    }
    found
}

/// Is `ip` on one of this PC's interfaces (a PC with its own public address)?
fn seen_is_mine(ip: Option<Ipv4Addr>) -> bool {
    ip.is_some_and(|ip| crate::linkpreview::ssrf::interface_addresses().iter().any(|(a, _)| *a == IpAddr::V4(ip)))
}

/// While the server runs: renew the mappings; when it is asked to stop, remove them.
async fn keep_mapped(
    mut stop_rx: watch::Receiver<bool>,
    mapper: Option<Arc<dyn PortMapper>>,
    block: PortBlock,
    local: Option<Ipv4Addr>,
    mut mapped: Option<Mapped>,
    status: Arc<Mutex<HostStatus>>,
    wake: Wake,
) {
    let mut tick = tokio::time::interval(upnp::RENEW_EVERY);
    tick.tick().await;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let (Some(m), Some(l), Some(prev)) = (mapper.clone(), local, mapped.clone()) else { continue };
                let renewed = tokio::task::spawn_blocking(move || upnp::map_block(m.as_ref(), &block, l, prev.permanent)).await;
                if let Ok(r) = renewed {
                    if !r.ports.is_empty() {
                        set(&status, &wake, |s| s.upnp = UpnpState::Mapped(r.clone()));
                        mapped = Some(r);
                    }
                }
            }
            _ = crate::state::wait_shutdown(&mut stop_rx) => break,
        }
    }
    tracing::info!("host: stopping");
    set(&status, &wake, |s| s.phase = Phase::Stopping);
    cleanup(mapper, mapped).await;
}

async fn cleanup(mapper: Option<Arc<dyn PortMapper>>, mapped: Option<Mapped>) {
    let (Some(m), Some(mapped)) = (mapper, mapped) else { return };
    let undo = tokio::task::spawn_blocking(move || upnp::unmap(m.as_ref(), &mapped));
    if tokio::time::timeout(Duration::from_secs(5), undo).await.is_err() {
        tracing::warn!("host: removing the port mappings took too long");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_are_created_listed_and_reloaded() {
        let root = tempfile::tempdir().unwrap();
        assert!(list(root.path()).is_empty());
        let a = create(root.path(), "  Casa da Tati \"oficial\" \\ ").unwrap();
        assert_eq!(a.name, "Casa da Tati \"oficial\" \\");
        assert_eq!(a.ports, PortBlock::DEFAULT);
        assert_eq!(a.password.len(), 16);
        std::fs::create_dir_all(root.path().join("bin")).unwrap();
        let b = create(root.path(), "Joao").unwrap();
        assert_ne!(a.id, b.id);
        assert_ne!(a.password, b.password);
        let all = list(root.path());
        assert_eq!(all.len(), 2);
        assert!(all.contains(&a) && all.contains(&b));
        // The hub reads the same file.
        let cfg = crate::config::load_or_create(&a.dir).unwrap();
        assert_eq!(cfg.server_name, a.name);
        assert_eq!(cfg.server_password, a.password);
        assert!(cfg.livekit.api_secret.len() >= 32 && cfg.livekit.api_key.starts_with("API"));
        assert_eq!(cfg.bind, "0.0.0.0:7890");
        assert!(create(root.path(), "   ").is_err());
        assert!(create(root.path(), &"x".repeat(NAME_MAX + 1)).is_err());
    }

    fn wait_for(r: &HostRunner, what: &str, f: impl Fn(&HostStatus) -> bool) -> HostStatus {
        let t = std::time::Instant::now();
        loop {
            let s = r.status();
            if f(&s) {
                return s;
            }
            assert!(t.elapsed() < Duration::from_secs(20), "{what}: {s:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// A hosted server on its own thread with a fake router: ports mapped while it runs, removed
    /// when it stops; the config is rewritten with the chosen ports and the public address.
    #[test]
    fn a_hosted_server_maps_its_ports_and_cleans_up() {
        use upnp::fake::FakeIgd;
        let root = tempfile::tempdir().unwrap();
        let h = create(root.path(), "Teste").unwrap();
        let router = Arc::new(FakeIgd { wan: Some("203.0.113.7".parse().unwrap()), ..Default::default() });
        let opts = RunOptions {
            net: NetMode::Off,
            mapper: Some(router.clone()),
            bind_ip: Ipv4Addr::LOCALHOST,
            first_block: 6,
            livekit_bin: None,
            lan_override: Some((Ipv4Addr::LOCALHOST, 8)),
        };
        let mut r = HostRunner::start(&h, opts, Arc::new(|| {}));
        let s = wait_for(&r, "running", |s| s.phase == Phase::Running);
        let block = PortBlock::nth(6);
        assert_eq!(s.ports, Some(block));
        assert_eq!(s.hub_addr, Some(SocketAddr::from((Ipv4Addr::LOCALHOST, block.hub))));
        assert_eq!(s.reach, Some(Reach::Open));
        assert_eq!(s.public_ipv4(), Some(Ipv4Addr::new(203, 0, 113, 7)));
        assert!(matches!(&s.upnp, UpnpState::Mapped(m) if m.complete()), "{:?}", s.upnp);
        assert_eq!(router.table.lock().unwrap().len(), 4);
        assert!(std::net::TcpStream::connect(s.hub_addr.unwrap()).is_ok());
        let cfg = crate::config::load_or_create(&h.dir).unwrap();
        assert_eq!(cfg.bind, format!("127.0.0.1:{}", block.hub));
        assert_eq!((cfg.public_host.as_str(), cfg.livekit.signal_port, cfg.livekit.use_external_ip), ("203.0.113.7", block.signal, false));
        assert!(r.is_live());
        r.stop_and_wait(Duration::from_secs(10));
        assert_eq!(r.status().phase, Phase::Stopped);
        assert!(router.table.lock().unwrap().is_empty(), "mappings removed on stop");
        assert!(std::net::TcpStream::connect(s.hub_addr.unwrap()).is_err(), "nothing listens any more");
        assert_eq!(load(&h.dir).unwrap().ports, block, "the ports are kept for next time");
    }

    #[test]
    fn invites_carry_the_reachable_addresses() {
        let h = Hosted { id: "x".into(), dir: PathBuf::new(), name: "s".into(), ports: PortBlock::DEFAULT, password: String::new(), accept_old_apps: true };
        let mut s = HostStatus::new(&h);
        s.lan = Some((Ipv4Addr::new(192, 168, 0, 20), 24));
        s.router_wan = Some("203.0.113.7".parse().unwrap());
        s.stun_ip = Some(Ipv4Addr::new(203, 0, 113, 7));
        s.public_v6 = Some("2001:db8::7".parse().unwrap());
        s.reach = Some(Reach::Open);
        assert_eq!(s.invite_addrs(), [InviteAddr::Lan(Ipv4Addr::new(192, 168, 0, 20), 24), InviteAddr::Public("203.0.113.7".parse().unwrap())]);
        // Behind CGNAT the router's address is useless; IPv6 is the way in.
        s.router_wan = Some("100.70.1.2".parse().unwrap());
        s.reach = Some(nat::classify(s.router_wan, Some("203.0.113.7".parse().unwrap()), false));
        assert_eq!(s.public_ipv4(), Some(Ipv4Addr::new(203, 0, 113, 7)));
        assert_eq!(s.invite_addrs().last(), Some(&InviteAddr::Public("2001:db8::7".parse().unwrap())));
        // Offline: only the LAN.
        let mut s = HostStatus::new(&h);
        s.lan = Some((Ipv4Addr::LOCALHOST, 8));
        assert_eq!(s.invite_addrs(), [InviteAddr::Lan(Ipv4Addr::LOCALHOST, 8)]);
    }
}
